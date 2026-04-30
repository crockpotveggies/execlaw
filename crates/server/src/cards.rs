//! Server-side helpers for emitting + reading the generic `Card`
//! primitive (see `execlaw_core::cards`).
//!
//! Long-running tools (research runner, future shell-session runner,
//! etc.) emit cards through these helpers rather than writing
//! `EventLog::commit_turn` directly. Three reasons:
//!
//!   1. Atomic commit semantics — every card-event commit goes
//!      through `commit_turn` so the per-conversation `last_seq`
//!      stays monotonic and the WS event bus picks the new event up
//!      naturally.
//!   2. Single point to enforce payload shape (the
//!      `CardOpenedPayload` / etc. structs) so we don't end up with
//!      malformed payloads slipping into the log.
//!   3. Future reach: lets us layer card-emit-side metrics, audit
//!      hooks, and approval gates here without retrofitting every
//!      caller.
//!
//! 2026-04-29.

use crate::events::{EventBus, UiEvent};
use execlaw_core::Database;
use execlaw_core::cards::{
    Card, CardClosedPayload, CardEvent, CardOpenedPayload, CardProgressedPayload,
};
use execlaw_core::events::{EventKind, EventLog, EventRecord, PendingEvent};
use execlaw_core::ids::{ConversationId, EventSeq};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CardEmitError {
    #[error(transparent)]
    Db(#[from] execlaw_core::DbError),
    #[error("encoding card payload: {0}")]
    Encode(String),
}

/// Commit a `CardOpened` event for `conversation_id`. The caller-
/// supplied `actor` is the principal id of whoever opened the card
/// (often `"system"` for runner-internal cards; `"controller"` for
/// operator-initiated ones).
pub fn open_card(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    payload: &CardOpenedPayload,
) -> Result<(), CardEmitError> {
    commit_card_event(db, cid, actor, EventKind::CardOpened, payload)
}

pub fn progress_card(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    payload: &CardProgressedPayload,
) -> Result<(), CardEmitError> {
    commit_card_event(db, cid, actor, EventKind::CardProgressed, payload)
}

pub fn close_card(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    payload: &CardClosedPayload,
) -> Result<(), CardEmitError> {
    commit_card_event(db, cid, actor, EventKind::CardClosed, payload)
}

/// Commit + broadcast variants. Used by the research subsystem so
/// the SPA's chat-pane sees card lifecycle events live; legacy
/// callers (and tests with no bus) keep using `open_card`/etc. The
/// commit happens FIRST so the durable source of truth (the
/// `state_events` row) lands before the SPA gets the live notification
/// — a subscriber that loses the WS in the middle of a job can
/// always re-project from the log.
pub fn open_card_and_broadcast(
    db: &Database,
    bus: &EventBus,
    cid: &ConversationId,
    actor: &str,
    payload: &CardOpenedPayload,
) -> Result<(), CardEmitError> {
    open_card(db, cid, actor, payload)?;
    let now = chrono::Utc::now().timestamp();
    let actions_json =
        serde_json::to_value(&payload.actions).unwrap_or_else(|_| serde_json::json!([]));
    bus.publish(UiEvent::CardOpened {
        conversation_id: cid.as_str().to_owned(),
        card_id: payload.card_id.clone(),
        card_kind: payload.kind.as_str().to_owned(),
        title: payload.title.clone(),
        summary: payload.summary.clone(),
        state: payload.state.map(|s| s.as_str().to_owned()),
        details: payload.details.clone(),
        actions: actions_json,
        committed_at: now,
    });
    Ok(())
}

pub fn progress_card_and_broadcast(
    db: &Database,
    bus: &EventBus,
    cid: &ConversationId,
    actor: &str,
    payload: &CardProgressedPayload,
) -> Result<(), CardEmitError> {
    progress_card(db, cid, actor, payload)?;
    let now = chrono::Utc::now().timestamp();
    let actions_json = payload
        .actions
        .as_ref()
        .and_then(|a| serde_json::to_value(a).ok());
    bus.publish(UiEvent::CardProgressed {
        conversation_id: cid.as_str().to_owned(),
        card_id: payload.card_id.clone(),
        state: payload.state.map(|s| s.as_str().to_owned()),
        progress: payload.progress,
        phase: payload.phase.clone(),
        details: payload.details.clone(),
        actions: actions_json,
        summary: payload.summary.clone(),
        committed_at: now,
    });
    Ok(())
}

pub fn close_card_and_broadcast(
    db: &Database,
    bus: &EventBus,
    cid: &ConversationId,
    actor: &str,
    payload: &CardClosedPayload,
) -> Result<(), CardEmitError> {
    close_card(db, cid, actor, payload)?;
    let now = chrono::Utc::now().timestamp();
    bus.publish(UiEvent::CardClosed {
        conversation_id: cid.as_str().to_owned(),
        card_id: payload.card_id.clone(),
        state: payload.state.as_str().to_owned(),
        summary: payload.summary.clone(),
        details: payload.details.clone(),
        attachment_id: payload.attachment_id.clone(),
        error: payload.error.clone(),
        committed_at: now,
    });
    Ok(())
}

fn commit_card_event<T: serde::Serialize>(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    kind: EventKind,
    payload: &T,
) -> Result<(), CardEmitError> {
    let log = EventLog::new(db);
    let base_seq = log.last_seq(cid)?;
    let pending = PendingEvent {
        kind,
        payload: rmp_serde::to_vec(payload).map_err(|e| CardEmitError::Encode(e.to_string()))?,
        actor: Some(actor.to_owned()),
    };
    log.commit_turn(cid, base_seq, vec![pending])?;
    Ok(())
}

/// Build the live projection of a single card by replaying every
/// `card.*` event for its conversation that references `card_id`.
/// Used by `/api/admin/cards/<conversation_id>/<card_id>` (forthcoming)
/// and by tests verifying end-to-end card lifecycles.
pub fn project_card(
    db: &Database,
    cid: &ConversationId,
    card_id: &str,
) -> Result<Option<Card>, CardEmitError> {
    let log = EventLog::new(db);
    let events = log.replay_since(cid, EventSeq(0))?;
    let mut card: Option<Card> = None;
    for ev in events {
        let card_event = match ev.kind {
            EventKind::CardOpened => {
                let p: CardOpenedPayload = decode_payload(&ev)?;
                if p.card_id != card_id {
                    continue;
                }
                CardEvent::Opened(p, ev.committed_at)
            }
            EventKind::CardProgressed => {
                let p: CardProgressedPayload = decode_payload(&ev)?;
                if p.card_id != card_id {
                    continue;
                }
                CardEvent::Progressed(p, ev.committed_at)
            }
            EventKind::CardClosed => {
                let p: CardClosedPayload = decode_payload(&ev)?;
                if p.card_id != card_id {
                    continue;
                }
                CardEvent::Closed(p, ev.committed_at)
            }
            _ => continue,
        };
        match (&mut card, &card_event) {
            (None, CardEvent::Opened(..)) => {
                card = Card::from_opened(cid.as_str(), &card_event);
            }
            (Some(c), _) => {
                c.apply(&card_event);
            }
            // Progressed/Closed seen before Opened — log-replay is
            // strictly ordered by seq, so this can only happen if a
            // tool incorrectly committed the wrong order. Tolerate
            // (skip), since strict-fail would deny operator visibility
            // for a partially-broken card.
            (None, _) => continue,
        }
    }
    Ok(card)
}

fn decode_payload<T: serde::de::DeserializeOwned>(ev: &EventRecord) -> Result<T, CardEmitError> {
    rmp_serde::from_slice(&ev.payload).map_err(|e| CardEmitError::Encode(format!("decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::cards::{CardKind, CardState};
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_conv(db: &Database, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(db)
            .upsert(&ConversationRow {
                conversation_id: cid.clone(),
                kind: ConversationKind::ControllerDM,
                last_seq: EventSeq(0),
                phase: Phase::Idle,
                controller_id: None,
                trust_class: "Controller".into(),
                snapshot_blob: None,
                snapshot_seq: None,
                lease_owner: None,
                lease_expires: None,
                modality: Modality::Text,
                display_name: None,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    /// Open → progress → close round-trips through the event log
    /// and projects to the expected terminal state.
    #[test]
    fn full_card_lifecycle_projects_correctly() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c1");

        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-1".into(),
                kind: CardKind::LongRunningTask,
                title: "Test task".into(),
                summary: "Started".into(),
                state: None,
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();

        progress_card(
            &db,
            &cid,
            "system",
            &CardProgressedPayload {
                card_id: "card-1".into(),
                state: Some(CardState::Running),
                progress: Some(0.5),
                phase: Some("Halfway".into()),
                details: None,
                actions: None,
                summary: Some("50%".into()),
            },
        )
        .unwrap();

        close_card(
            &db,
            &cid,
            "system",
            &CardClosedPayload {
                card_id: "card-1".into(),
                state: CardState::Completed,
                summary: "Done.".into(),
                details: None,
                attachment_id: None,
                error: None,
            },
        )
        .unwrap();

        let card = project_card(&db, &cid, "card-1").unwrap().unwrap();
        assert_eq!(card.state, CardState::Completed);
        assert_eq!(card.progress, Some(0.5));
        assert_eq!(card.phase.as_deref(), Some("Halfway"));
        assert_eq!(card.summary, "Done.");
    }

    /// Card events for unrelated cards must NOT bleed into the
    /// projection.
    #[test]
    fn projection_filters_by_card_id() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c2");
        for n in 0..3 {
            open_card(
                &db,
                &cid,
                "system",
                &CardOpenedPayload {
                    card_id: format!("card-{n}"),
                    kind: CardKind::LongRunningTask,
                    title: format!("Task {n}"),
                    summary: "x".into(),
                    state: None,
                    details: serde_json::json!({"n": n}),
                    actions: vec![],
                },
            )
            .unwrap();
        }
        // Progress only the middle one.
        progress_card(
            &db,
            &cid,
            "system",
            &CardProgressedPayload {
                card_id: "card-1".into(),
                state: Some(CardState::Running),
                progress: Some(0.7),
                phase: None,
                details: None,
                actions: None,
                summary: None,
            },
        )
        .unwrap();
        let one = project_card(&db, &cid, "card-1").unwrap().unwrap();
        let zero = project_card(&db, &cid, "card-0").unwrap().unwrap();
        assert_eq!(one.progress, Some(0.7));
        // card-0 saw an Open + nothing else; should still be Pending
        // with no progress.
        assert_eq!(zero.progress, None);
        assert_eq!(zero.state, CardState::Pending);
    }

    /// Looking up a card_id that never had a CardOpened event
    /// returns `None` — the projection layer never invents a Card
    /// from out-of-band Progressed events.
    #[test]
    fn projection_returns_none_for_unknown_card() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c3");
        let card = project_card(&db, &cid, "ghost").unwrap();
        assert!(card.is_none());
    }

    /// Card events monotonically increment `last_seq` like every
    /// other commit_turn call — proves the integration with the
    /// existing event-log invariants.
    #[test]
    fn card_events_advance_conversation_last_seq() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c4");
        let log = EventLog::new(&db);
        assert_eq!(log.last_seq(&cid).unwrap(), EventSeq(0));
        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-x".into(),
                kind: CardKind::LongRunningTask,
                title: "T".into(),
                summary: "S".into(),
                state: None,
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();
        let after_open = log.last_seq(&cid).unwrap();
        assert!(after_open.0 > 0);
        progress_card(
            &db,
            &cid,
            "system",
            &CardProgressedPayload {
                card_id: "card-x".into(),
                state: None,
                progress: Some(0.1),
                phase: None,
                details: None,
                actions: None,
                summary: None,
            },
        )
        .unwrap();
        let after_progress = log.last_seq(&cid).unwrap();
        assert!(after_progress.0 > after_open.0);
    }
}
