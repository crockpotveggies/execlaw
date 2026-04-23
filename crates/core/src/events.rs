//! Event log primitives (§2.3, §2.4).
//!
//! The `state_events` table is append-only. Reads replay events in order to
//! reconstruct conversation state. Turns commit atomically — see `commit_turn`
//! which enforces the `tool_use`/`tool_result` pairing invariant by
//! synthesizing cancellation results for any open tool_use without a
//! matching result.

use crate::db::{Database, DbError};
use crate::ids::{ConversationId, EventSeq};
use rmp_serde as rmps;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The set of event kinds a replay must be able to reconstruct.
///
/// This is **additive** — new kinds land without breaking existing consumers
/// because replay is expected to tolerate unknown kinds by skipping them
/// for state-reconstruction purposes while still carrying them forward in
/// forensic queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    UserMsg,
    ModelTurn,
    ToolUse,
    ToolResult,
    Interrupt,
    Resume,
    Approval,
    EffectCommitted,
    Wakeup,
    AlertFired,
    AlertRenotified,
    AlertAcked,
    AlertResolved,
    AlertSnoozed,
    IncidentOpened,
    IncidentClosed,
    ColdContactArrived,
    IdentityResolutionConflict,
    TrustChanged,
    VoiceSessionStarted,
    VoiceSessionEnded,
    VadSpeechStarted,
    VadSpeechEnded,
    SttPartial,
    SttFinal,
    TurnUserEnded,
    LlmToken,
    LlmResponseFinal,
    LlmCancelled,
    TtsFirstAudio,
    TtsAudioChunk,
    TtsEnded,
    InterruptStarted,
    InterruptRescinded,
    InterruptConfirmed,
    ResearchProgressUpdated,
    /// Escape hatch for future additions that predate this enum. The payload
    /// contains the original kind string.
    Other,
}

impl EventKind {
    /// Canonical wire form used in `state_events.kind`.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::UserMsg => "user_msg",
            EventKind::ModelTurn => "model_turn",
            EventKind::ToolUse => "tool_use",
            EventKind::ToolResult => "tool_result",
            EventKind::Interrupt => "interrupt",
            EventKind::Resume => "resume",
            EventKind::Approval => "approval",
            EventKind::EffectCommitted => "effect_committed",
            EventKind::Wakeup => "wakeup",
            EventKind::AlertFired => "alert_fired",
            EventKind::AlertRenotified => "alert_renotified",
            EventKind::AlertAcked => "alert_acked",
            EventKind::AlertResolved => "alert_resolved",
            EventKind::AlertSnoozed => "alert_snoozed",
            EventKind::IncidentOpened => "incident_opened",
            EventKind::IncidentClosed => "incident_closed",
            EventKind::ColdContactArrived => "cold_contact_arrived",
            EventKind::IdentityResolutionConflict => "identity_resolution_conflict",
            EventKind::TrustChanged => "trust_changed",
            EventKind::VoiceSessionStarted => "voice.session_started",
            EventKind::VoiceSessionEnded => "voice.session_ended",
            EventKind::VadSpeechStarted => "vad.speech_started",
            EventKind::VadSpeechEnded => "vad.speech_ended",
            EventKind::SttPartial => "stt.partial",
            EventKind::SttFinal => "stt.final",
            EventKind::TurnUserEnded => "turn.user_ended",
            EventKind::LlmToken => "llm.token",
            EventKind::LlmResponseFinal => "llm.response_final",
            EventKind::LlmCancelled => "llm.cancelled",
            EventKind::TtsFirstAudio => "tts.first_audio",
            EventKind::TtsAudioChunk => "tts.audio_chunk",
            EventKind::TtsEnded => "tts.ended",
            EventKind::InterruptStarted => "interrupt.started",
            EventKind::InterruptRescinded => "interrupt.rescinded",
            EventKind::InterruptConfirmed => "interrupt.confirmed",
            EventKind::ResearchProgressUpdated => "research_progress_updated",
            EventKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "user_msg" => EventKind::UserMsg,
            "model_turn" => EventKind::ModelTurn,
            "tool_use" => EventKind::ToolUse,
            "tool_result" => EventKind::ToolResult,
            "interrupt" => EventKind::Interrupt,
            "resume" => EventKind::Resume,
            "approval" => EventKind::Approval,
            "effect_committed" => EventKind::EffectCommitted,
            "wakeup" => EventKind::Wakeup,
            "alert_fired" => EventKind::AlertFired,
            "alert_renotified" => EventKind::AlertRenotified,
            "alert_acked" => EventKind::AlertAcked,
            "alert_resolved" => EventKind::AlertResolved,
            "alert_snoozed" => EventKind::AlertSnoozed,
            "incident_opened" => EventKind::IncidentOpened,
            "incident_closed" => EventKind::IncidentClosed,
            "cold_contact_arrived" => EventKind::ColdContactArrived,
            "identity_resolution_conflict" => EventKind::IdentityResolutionConflict,
            "trust_changed" => EventKind::TrustChanged,
            "voice.session_started" => EventKind::VoiceSessionStarted,
            "voice.session_ended" => EventKind::VoiceSessionEnded,
            "vad.speech_started" => EventKind::VadSpeechStarted,
            "vad.speech_ended" => EventKind::VadSpeechEnded,
            "stt.partial" => EventKind::SttPartial,
            "stt.final" => EventKind::SttFinal,
            "turn.user_ended" => EventKind::TurnUserEnded,
            "llm.token" => EventKind::LlmToken,
            "llm.response_final" => EventKind::LlmResponseFinal,
            "llm.cancelled" => EventKind::LlmCancelled,
            "tts.first_audio" => EventKind::TtsFirstAudio,
            "tts.audio_chunk" => EventKind::TtsAudioChunk,
            "tts.ended" => EventKind::TtsEnded,
            "interrupt.started" => EventKind::InterruptStarted,
            "interrupt.rescinded" => EventKind::InterruptRescinded,
            "interrupt.confirmed" => EventKind::InterruptConfirmed,
            "research_progress_updated" => EventKind::ResearchProgressUpdated,
            _ => EventKind::Other,
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row in `state_events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub conversation_id: ConversationId,
    pub seq: EventSeq,
    pub kind: EventKind,
    pub payload: Vec<u8>, // MessagePack-encoded
    pub committed_at: i64,
    pub actor: Option<String>,
}

impl EventRecord {
    /// Build a new event record; the caller is responsible for passing a
    /// `seq` that is strictly greater than the conversation's current
    /// `last_seq`.
    pub fn new<P: Serialize>(
        conversation_id: ConversationId,
        seq: EventSeq,
        kind: EventKind,
        payload: &P,
        actor: Option<String>,
    ) -> Result<Self, DbError> {
        let payload = rmps::to_vec_named(payload)
            .map_err(|e| DbError::Serde(format!("encoding event payload: {e}")))?;
        Ok(Self {
            conversation_id,
            seq,
            kind,
            payload,
            committed_at: chrono::Utc::now().timestamp(),
            actor,
        })
    }

    /// Decode the MessagePack payload into a typed struct.
    pub fn decode_payload<P: for<'de> Deserialize<'de>>(&self) -> Result<P, DbError> {
        rmps::from_slice(&self.payload)
            .map_err(|e| DbError::Serde(format!("decoding event payload: {e}")))
    }
}

/// Minimal envelope for a `tool_use` event when we need to correlate with a
/// matching `tool_result` to enforce the pairing invariant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsePayload {
    /// Framework-assigned ordinal (the `tool_call_ordinal` used in the
    /// outbox idempotency key). Stable across retries.
    pub ordinal: u32,
    pub tool_name: String,
    pub args_json: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    /// Must match an upstream `ToolUsePayload.ordinal`.
    pub ordinal: u32,
    /// `Ok(_)` on success, `Err(reason)` on failure or synthesized cancel.
    pub outcome: Result<serde_json::Value, String>,
}

/// The event log facade.
pub struct EventLog<'db> {
    db: &'db Database,
}

impl<'db> EventLog<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Append one event. Enforces `(conversation_id, seq)` uniqueness via
    /// the primary key — returns an error if the caller passed a stale seq.
    pub fn append(&self, ev: &EventRecord) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_events \
                 (conversation_id, seq, kind, payload, committed_at, actor) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    ev.conversation_id.as_str(),
                    ev.seq.0,
                    ev.kind.as_str(),
                    ev.payload,
                    ev.committed_at,
                    ev.actor,
                ],
            )?;
            Ok(())
        })
    }

    /// Read events for a conversation strictly greater than `after_seq`,
    /// in ascending order.
    pub fn replay_since(
        &self,
        conversation_id: &ConversationId,
        after_seq: EventSeq,
    ) -> Result<Vec<EventRecord>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT seq, kind, payload, committed_at, actor \
                 FROM state_events \
                 WHERE conversation_id = ?1 AND seq > ?2 \
                 ORDER BY seq ASC",
            )?;
            let rows = stmt
                .query_map(params![conversation_id.as_str(), after_seq.0], |row| {
                    let seq: i64 = row.get(0)?;
                    let kind: String = row.get(1)?;
                    let payload: Vec<u8> = row.get(2)?;
                    let committed_at: i64 = row.get(3)?;
                    let actor: Option<String> = row.get(4)?;
                    Ok(EventRecord {
                        conversation_id: conversation_id.clone(),
                        seq: EventSeq(seq),
                        kind: EventKind::parse(&kind),
                        payload,
                        committed_at,
                        actor,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Current last_seq for a conversation (0 if none yet).
    pub fn last_seq(&self, conversation_id: &ConversationId) -> Result<EventSeq, DbError> {
        self.db.with_conn(|c| {
            let got: Option<i64> = c
                .query_row(
                    "SELECT MAX(seq) FROM state_events WHERE conversation_id = ?1",
                    params![conversation_id.as_str()],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            Ok(EventSeq(got.unwrap_or(0)))
        })
    }

    /// Commit a turn atomically: all events either land together or none do.
    ///
    /// Enforces §2.2 axiom #3: every `tool_use` must pair with a
    /// `tool_result`. If the caller passes a `tool_use` whose ordinal is
    /// not matched by a `tool_result` in the same batch, we synthesize a
    /// cancellation result with `reason`.
    ///
    /// The returned vec contains the events actually written, including any
    /// synthesized cancellations, in order.
    pub fn commit_turn(
        &self,
        conversation_id: &ConversationId,
        base_seq: EventSeq,
        events: Vec<PendingEvent>,
    ) -> Result<Vec<EventRecord>, DbError> {
        let materialized = enforce_tool_pairing(conversation_id, base_seq, events)?;

        self.db.transaction(|tx| {
            for ev in &materialized {
                tx.execute(
                    "INSERT INTO state_events \
                     (conversation_id, seq, kind, payload, committed_at, actor) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        ev.conversation_id.as_str(),
                        ev.seq.0,
                        ev.kind.as_str(),
                        ev.payload,
                        ev.committed_at,
                        ev.actor,
                    ],
                )?;
            }
            Ok(())
        })?;

        Ok(materialized)
    }
}

/// An event the caller wants to append. Seq is assigned by `commit_turn`.
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub kind: EventKind,
    pub payload: Vec<u8>,
    pub actor: Option<String>,
}

impl PendingEvent {
    pub fn encode<P: Serialize>(
        kind: EventKind,
        payload: &P,
        actor: Option<String>,
    ) -> Result<Self, DbError> {
        let payload = rmps::to_vec_named(payload)
            .map_err(|e| DbError::Serde(format!("encoding event payload: {e}")))?;
        Ok(Self {
            kind,
            payload,
            actor,
        })
    }
}

/// Walk the proposed event list; for every `tool_use(ordinal = N)` without
/// a later `tool_result(ordinal = N)`, insert a synthesized cancellation
/// result right after the `tool_use`.
fn enforce_tool_pairing(
    conversation_id: &ConversationId,
    base_seq: EventSeq,
    events: Vec<PendingEvent>,
) -> Result<Vec<EventRecord>, DbError> {
    // Index tool_result ordinals so we can detect missing matches.
    let mut results_by_ordinal: std::collections::HashSet<u32> = Default::default();
    for ev in &events {
        if ev.kind == EventKind::ToolResult {
            let decoded: ToolResultPayload = rmps::from_slice(&ev.payload)
                .map_err(|e| DbError::Serde(format!("decoding tool_result: {e}")))?;
            results_by_ordinal.insert(decoded.ordinal);
        }
    }

    let mut out: Vec<EventRecord> = Vec::with_capacity(events.len() + 2);
    let mut next_seq = base_seq.next();
    let now = chrono::Utc::now().timestamp();

    for ev in events {
        if ev.kind == EventKind::ToolUse {
            let parsed: ToolUsePayload = rmps::from_slice(&ev.payload)
                .map_err(|e| DbError::Serde(format!("decoding tool_use: {e}")))?;

            // Write the tool_use.
            out.push(EventRecord {
                conversation_id: conversation_id.clone(),
                seq: next_seq,
                kind: EventKind::ToolUse,
                payload: ev.payload.clone(),
                committed_at: now,
                actor: ev.actor.clone(),
            });
            next_seq = next_seq.next();

            // If there's no matching tool_result in the batch, synthesize one.
            if !results_by_ordinal.contains(&parsed.ordinal) {
                let synthetic = ToolResultPayload {
                    ordinal: parsed.ordinal,
                    outcome: Err(format!(
                        "cancelled: no tool_result in same commit for ordinal {}",
                        parsed.ordinal
                    )),
                };
                let payload = rmps::to_vec_named(&synthetic)
                    .map_err(|e| DbError::Serde(format!("encoding synthetic tool_result: {e}")))?;
                out.push(EventRecord {
                    conversation_id: conversation_id.clone(),
                    seq: next_seq,
                    kind: EventKind::ToolResult,
                    payload,
                    committed_at: now,
                    actor: Some("system".to_owned()),
                });
                next_seq = next_seq.next();
            }
        } else {
            out.push(EventRecord {
                conversation_id: conversation_id.clone(),
                seq: next_seq,
                kind: ev.kind,
                payload: ev.payload,
                committed_at: now,
                actor: ev.actor,
            });
            next_seq = next_seq.next();
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;
    use serde_json::json;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn append_and_replay_roundtrip() {
        let db = fresh_db();
        let log = EventLog::new(&db);
        let cid = ConversationId::from("conv-rt");

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct P {
            text: String,
        }

        let ev = EventRecord::new(
            cid.clone(),
            EventSeq::FIRST,
            EventKind::UserMsg,
            &P { text: "hi".into() },
            Some("controller".into()),
        )
        .unwrap();
        log.append(&ev).unwrap();

        let got = log.replay_since(&cid, EventSeq(0)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, EventKind::UserMsg);
        let decoded: P = got[0].decode_payload().unwrap();
        assert_eq!(decoded.text, "hi");
    }

    #[test]
    fn last_seq_starts_at_zero() {
        let db = fresh_db();
        let log = EventLog::new(&db);
        let cid = ConversationId::new();
        assert_eq!(log.last_seq(&cid).unwrap(), EventSeq(0));
    }

    #[test]
    fn commit_turn_synthesizes_missing_tool_result() {
        let db = fresh_db();
        let log = EventLog::new(&db);
        let cid = ConversationId::from("conv-pair");

        let turn_events = vec![
            PendingEvent::encode(
                EventKind::ModelTurn,
                &json!({"text": "let me check"}),
                Some("agent".into()),
            )
            .unwrap(),
            PendingEvent::encode(
                EventKind::ToolUse,
                &ToolUsePayload {
                    ordinal: 0,
                    tool_name: "list_events".into(),
                    args_json: json!({}),
                },
                Some("agent".into()),
            )
            .unwrap(),
            // DELIBERATELY no ToolResult — commit_turn must synthesize one.
        ];

        let written = log.commit_turn(&cid, EventSeq(0), turn_events).unwrap();
        assert_eq!(
            written.len(),
            3,
            "expected ModelTurn + ToolUse + synthetic ToolResult"
        );
        assert_eq!(written[0].kind, EventKind::ModelTurn);
        assert_eq!(written[1].kind, EventKind::ToolUse);
        assert_eq!(written[2].kind, EventKind::ToolResult);

        let synthetic: ToolResultPayload = written[2].decode_payload().unwrap();
        assert!(synthetic.outcome.is_err());
        assert_eq!(synthetic.ordinal, 0);
    }

    #[test]
    fn commit_turn_leaves_matched_pairs_untouched() {
        let db = fresh_db();
        let log = EventLog::new(&db);
        let cid = ConversationId::from("conv-match");

        let turn_events = vec![
            PendingEvent::encode(
                EventKind::ToolUse,
                &ToolUsePayload {
                    ordinal: 7,
                    tool_name: "ping".into(),
                    args_json: json!({}),
                },
                Some("agent".into()),
            )
            .unwrap(),
            PendingEvent::encode(
                EventKind::ToolResult,
                &ToolResultPayload {
                    ordinal: 7,
                    outcome: Ok(json!({"pong": true})),
                },
                Some("system".into()),
            )
            .unwrap(),
        ];

        let written = log.commit_turn(&cid, EventSeq(0), turn_events).unwrap();
        assert_eq!(written.len(), 2);

        let res: ToolResultPayload = written[1].decode_payload().unwrap();
        assert!(res.outcome.is_ok());
    }

    #[test]
    fn append_refuses_duplicate_seq() {
        let db = fresh_db();
        let log = EventLog::new(&db);
        let cid = ConversationId::from("dup");
        let ev1 = EventRecord::new(
            cid.clone(),
            EventSeq::FIRST,
            EventKind::UserMsg,
            &serde_json::json!({"x": 1}),
            None,
        )
        .unwrap();
        let ev2 = EventRecord::new(
            cid.clone(),
            EventSeq::FIRST,
            EventKind::UserMsg,
            &serde_json::json!({"x": 2}),
            None,
        )
        .unwrap();
        log.append(&ev1).unwrap();
        let err = log.append(&ev2).unwrap_err();
        assert!(
            matches!(err, DbError::Sqlite(_)),
            "duplicate seq should fail with sqlite uniqueness error, got {:?}",
            err
        );
    }
}
