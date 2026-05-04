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
) -> Result<i64, CardEmitError> {
    commit_card_event(db, cid, actor, EventKind::CardOpened, payload)
}

pub fn progress_card(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    payload: &CardProgressedPayload,
) -> Result<i64, CardEmitError> {
    commit_card_event(db, cid, actor, EventKind::CardProgressed, payload)
}

pub fn close_card(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    payload: &CardClosedPayload,
) -> Result<i64, CardEmitError> {
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
    let event_seq = open_card(db, cid, actor, payload)?;
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
        event_seq,
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
    let event_seq = progress_card(db, cid, actor, payload)?;
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
        event_seq,
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
    let event_seq = close_card(db, cid, actor, payload)?;
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
        event_seq,
    });
    Ok(())
}

fn commit_card_event<T: serde::Serialize>(
    db: &Database,
    cid: &ConversationId,
    actor: &str,
    kind: EventKind,
    payload: &T,
) -> Result<i64, CardEmitError> {
    let log = EventLog::new(db);
    let base_seq = log.last_seq(cid)?;
    let pending = PendingEvent {
        kind,
        payload: rmp_serde::to_vec(payload).map_err(|e| CardEmitError::Encode(e.to_string()))?,
        actor: Some(actor.to_owned()),
    };
    let written = log.commit_turn(cid, base_seq, vec![pending])?;
    // commit_turn returns the materialized events with their assigned
    // seqs. We always commit exactly one card event per call, so
    // grab the last seq (defensive against future tool-pairing
    // synthesis prepending cancellation results before our event).
    let seq = written
        .last()
        .map(|e| e.seq.0)
        .unwrap_or(base_seq.0 + 1);
    Ok(seq)
}

/// Replay every `card.*` event in `cid`'s log and return the
/// reconstructed `Card` projection for each card_id seen. Used by
/// the SPA's `GET /api/chats/{cid}/cards` history endpoint so a
/// page refresh re-hydrates inline cards (research card,
/// attachment chip, etc.) — without this, cards were live-only
/// state in `cardStore` and vanished on refresh, even though the
/// underlying CardOpened/Closed events were durably persisted.
///
/// Returns the cards in opened-at order so the SPA can stream
/// them directly into its store without resorting.
pub fn project_cards_for_conversation(
    db: &Database,
    cid: &ConversationId,
) -> Result<Vec<Card>, CardEmitError> {
    let log = EventLog::new(db);
    let events = log.replay_since(cid, EventSeq(0))?;
    // Card projection is keyed by card_id; we rebuild each card by
    // applying its event sequence in order. A HashMap keeps the
    // build O(N events) without per-card scans through the rest of
    // the log; we materialise to a Vec at the end and sort by
    // opened_at so the SPA's chronological render matches the live
    // bus ordering.
    let mut by_id: std::collections::HashMap<String, Card> =
        std::collections::HashMap::new();
    for ev in events {
        let ev_seq = ev.seq.0;
        let (card_id, card_event) = match ev.kind {
            EventKind::CardOpened => {
                let p: CardOpenedPayload = decode_payload(&ev)?;
                let id = p.card_id.clone();
                (id, CardEvent::Opened(p, ev.committed_at))
            }
            EventKind::CardProgressed => {
                let p: CardProgressedPayload = decode_payload(&ev)?;
                let id = p.card_id.clone();
                (id, CardEvent::Progressed(p, ev.committed_at))
            }
            EventKind::CardClosed => {
                let p: CardClosedPayload = decode_payload(&ev)?;
                let id = p.card_id.clone();
                (id, CardEvent::Closed(p, ev.committed_at))
            }
            _ => continue,
        };
        // 2026-05-04 (rev 10): event_seq is the card's "surface
        // seq" — the seq it sorts by in the SPA's chat-pane
        // timeline. Updated on:
        //   * CardOpened (initial OR resume after clarification —
        //     the runner emits a duplicate Open with a fresh seq,
        //     so the card pops to the bottom)
        //   * CardClosed (one-time pop on completion so the
        //     deliverable surfaces inline with the latest
        //     activity)
        // NOT updated on CardProgressed (intermediate Gather/
        // Synth ticks would otherwise pop the card on every
        // sub-query, which is too noisy).
        let bumps_surface_seq =
            matches!(card_event, CardEvent::Opened(..) | CardEvent::Closed(..));
        if let Some(c) = by_id.get_mut(&card_id) {
            c.apply(&card_event);
            if bumps_surface_seq {
                c.event_seq = Some(ev_seq);
            }
        } else if let CardEvent::Opened(..) = &card_event
            && let Some(mut c) = Card::from_opened(cid.as_str(), &card_event)
        {
            c.event_seq = Some(ev_seq);
            by_id.insert(card_id, c);
        }
        // Progressed/Closed before Opened: skip. Same tolerance
        // as `project_card`.
    }
    let mut out: Vec<Card> = by_id.into_values().collect();
    // Sort by (opened_at, card_id). card_id is the stable
    // tiebreaker — opened_at is Unix-seconds and two cards opened
    // in the same tick (the runner often emits CardOpened back-
    // to-back during a single phase) would otherwise return in
    // non-deterministic HashMap iteration order.
    out.sort_by(|a, b| a.opened_at.cmp(&b.opened_at).then_with(|| a.card_id.cmp(&b.card_id)));
    Ok(out)
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
        let ev_seq = ev.seq.0;
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
                card = Card::from_opened(cid.as_str(), &card_event)
                    .map(|mut c| { c.event_seq = Some(ev_seq); c });
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
        // 2026-05-04: phase no longer survives a Closed event.
        // Apply for Closed re-derives phase from details.phase
        // (None here because the test's Closed payload has
        // details=None and the open-time details were `{}`). The
        // old behaviour where the last Progressed phase
        // ("Halfway") leaked past the close was the bug
        // surfaced by users seeing "Synthesizing" rendered below
        // a "Completed" badge.
        assert_eq!(card.phase, None);
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
    /// 2026-05-04 regression: card history endpoint replays the
    /// log into a Vec<Card> so the SPA can re-hydrate inline
    /// cards (research card, attachment chip, etc.) on page
    /// refresh. Asserts:
    ///   * each opened card_id appears once in the result
    ///   * Progressed/Closed events are merged into the right card
    ///   * order is opened_at ascending
    #[test]
    fn project_cards_for_conversation_returns_every_card_with_merged_events() {
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-history");

        // Card A: full lifecycle (Open + Progress + Close).
        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-a".into(),
                kind: CardKind::Research,
                title: "Research A".into(),
                summary: "starting".into(),
                state: Some(CardState::Running),
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
                card_id: "card-a".into(),
                state: Some(CardState::Running),
                progress: Some(0.5),
                phase: Some("gather".into()),
                details: None,
                actions: None,
                summary: Some("halfway".into()),
            },
        )
        .unwrap();
        close_card(
            &db,
            &cid,
            "system",
            &CardClosedPayload {
                card_id: "card-a".into(),
                state: CardState::Completed,
                summary: "done".into(),
                details: None,
                attachment_id: Some("att-a".into()),
                error: None,
            },
        )
        .unwrap();

        // Card B: just an Open (still Running).
        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-b".into(),
                kind: CardKind::Attachment,
                title: "report.pdf".into(),
                summary: "pdf".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({"attachment_id": "att-b"}),
                actions: vec![],
            },
        )
        .unwrap();

        let cards = project_cards_for_conversation(&db, &cid).unwrap();
        assert_eq!(cards.len(), 2, "two opened cards → two projection rows");

        // Ordered by opened_at ascending.
        assert_eq!(cards[0].card_id, "card-a");
        assert_eq!(cards[1].card_id, "card-b");

        // Card A reflects the closed terminal state + merged
        // progress.
        let a = &cards[0];
        assert_eq!(a.state, CardState::Completed);
        assert_eq!(a.summary, "done");
        assert_eq!(a.attachment_id.as_deref(), Some("att-a"));
        assert_eq!(a.progress, Some(0.5));

        // Card B is Running with the open-time details intact.
        let b = &cards[1];
        assert_eq!(b.state, CardState::Running);
        assert_eq!(b.kind, CardKind::Attachment);
    }

    /// 2026-05-04 regression: `apply` for CardClosed used to leave
    /// the `phase` field at whatever the last CardProgressed had
    /// set ("Synthesizing", "Gathering"), so the SPA showed a
    /// stale phase line below the "Completed" badge after the
    /// research card closed. The fix derives the post-close phase
    /// from `details.phase` when stamped (the deep-research path
    /// stamps "Complete"), else clears.
    #[test]
    fn closed_event_resets_phase_from_details() {
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-phase");
        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-x".into(),
                kind: CardKind::Research,
                title: "T".into(),
                summary: "starting".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({"phase": "Planning"}),
                actions: vec![],
            },
        )
        .unwrap();
        progress_card(
            &db,
            &cid,
            "system",
            &CardProgressedPayload {
                card_id: "card-x".into(),
                state: Some(CardState::Running),
                progress: Some(0.9),
                phase: Some("Synthesizing".into()),
                details: None,
                actions: None,
                summary: Some("nearly done".into()),
            },
        )
        .unwrap();
        // Sanity: phase is "Synthesizing" pre-close.
        let mid = project_card(&db, &cid, "card-x").unwrap().unwrap();
        assert_eq!(mid.phase.as_deref(), Some("Synthesizing"));

        close_card(
            &db,
            &cid,
            "system",
            &CardClosedPayload {
                card_id: "card-x".into(),
                state: CardState::Completed,
                summary: "done".into(),
                // Runner stamps phase=Complete in details on the
                // research path. Apply must pick that up so the
                // top-level phase field reflects the new state
                // instead of staying on "Synthesizing".
                details: Some(serde_json::json!({
                    "phase": "Complete",
                    "report_url": "/research/x",
                })),
                attachment_id: Some("att-x".into()),
                error: None,
            },
        )
        .unwrap();

        let after = project_card(&db, &cid, "card-x").unwrap().unwrap();
        assert_eq!(after.state, CardState::Completed);
        assert_eq!(
            after.phase.as_deref(),
            Some("Complete"),
            "phase must update from details.phase on Closed (was the stuck-on-Synthesizing bug)",
        );
        assert_eq!(after.attachment_id.as_deref(), Some("att-x"));
    }

    #[test]
    fn closed_event_clears_phase_when_details_omits_it() {
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-phase-clear");
        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "c".into(),
                kind: CardKind::Research,
                title: "T".into(),
                summary: "start".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({"phase": "Planning"}),
                actions: vec![],
            },
        )
        .unwrap();
        progress_card(
            &db,
            &cid,
            "system",
            &CardProgressedPayload {
                card_id: "c".into(),
                state: None,
                progress: None,
                phase: Some("Gathering".into()),
                details: None,
                actions: None,
                summary: None,
            },
        )
        .unwrap();
        close_card(
            &db,
            &cid,
            "system",
            &CardClosedPayload {
                card_id: "c".into(),
                state: CardState::Completed,
                summary: "done".into(),
                // No phase in details → apply clears the field.
                details: None,
                attachment_id: None,
                error: None,
            },
        )
        .unwrap();
        let after = project_card(&db, &cid, "c").unwrap().unwrap();
        // Closed apply doesn't touch self.details when payload's
        // details is None (which preserves the open-time
        // {"phase": "Planning"} payload). So phase reflects that.
        // The intent here is: phase MUST NOT stay on the stale
        // "Gathering" from the last Progressed.
        assert_ne!(
            after.phase.as_deref(),
            Some("Gathering"),
            "stale Progressed phase must not leak past close",
        );
    }

    /// 2026-05-04 — `event_seq` on the projected Card is the
    /// "surface seq" the SPA's chat-pane uses for inline ordering.
    /// Bumped on CardOpened (initial OR resume after clarification —
    /// the runner emits a duplicate Open with a fresh seq) and on
    /// CardClosed (one-time pop on completion). NOT bumped on
    /// CardProgressed (intermediate gather/synth ticks would
    /// otherwise pop the card on every sub-query, too noisy).
    #[test]
    fn project_event_seq_bumps_on_open_and_close_but_not_on_progressed() {
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-pop");

        // Open — initial seq.
        open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "c1".into(),
                kind: CardKind::Research,
                title: "T".into(),
                summary: "starting".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();
        let after_open = project_cards_for_conversation(&db, &cid).unwrap();
        let open_seq = after_open[0].event_seq.expect("event_seq populated on open");
        assert!(open_seq > 0);

        // Progressed — must NOT bump.
        progress_card(
            &db,
            &cid,
            "system",
            &CardProgressedPayload {
                card_id: "c1".into(),
                state: Some(CardState::Running),
                progress: Some(0.4),
                phase: Some("Gathering".into()),
                details: None,
                actions: None,
                summary: Some("mid".into()),
            },
        )
        .unwrap();
        let after_progress = project_cards_for_conversation(&db, &cid).unwrap();
        assert_eq!(
            after_progress[0].event_seq,
            Some(open_seq),
            "Progressed must not bump surface seq",
        );

        // Closed — must bump.
        close_card(
            &db,
            &cid,
            "system",
            &CardClosedPayload {
                card_id: "c1".into(),
                state: CardState::Completed,
                summary: "done".into(),
                details: None,
                attachment_id: Some("att-1".into()),
                error: None,
            },
        )
        .unwrap();
        let after_close = project_cards_for_conversation(&db, &cid).unwrap();
        let close_seq = after_close[0].event_seq.expect("event_seq still populated");
        assert!(
            close_seq > open_seq,
            "Closed must bump surface seq above the open seq (open={open_seq}, close={close_seq})",
        );
    }

    #[test]
    fn project_cards_for_conversation_returns_empty_for_conversation_with_no_cards() {
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-empty");
        let cards = project_cards_for_conversation(&db, &cid).unwrap();
        assert!(cards.is_empty());
    }

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
