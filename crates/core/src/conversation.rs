//! Conversation FSM rows (`state_conversations`, §2.3).

use crate::db::{Database, DbError};
use crate::ids::{ConversationId, EventSeq};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Conversation kind (§2.6). Derived from participant composition; stored
/// in the DB only as a shorthand / UI affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationKind {
    ControllerDM,
    GroupWithControllerPresent,
    GroupWithControllerAbsent,
    ExternalWithOutsider,
    MixedTrust,
}

impl ConversationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConversationKind::ControllerDM => "ControllerDM",
            ConversationKind::GroupWithControllerPresent => "GroupWithControllerPresent",
            ConversationKind::GroupWithControllerAbsent => "GroupWithControllerAbsent",
            ConversationKind::ExternalWithOutsider => "ExternalWithOutsider",
            ConversationKind::MixedTrust => "MixedTrust",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ControllerDM" => Some(Self::ControllerDM),
            "GroupWithControllerPresent" => Some(Self::GroupWithControllerPresent),
            "GroupWithControllerAbsent" => Some(Self::GroupWithControllerAbsent),
            "ExternalWithOutsider" => Some(Self::ExternalWithOutsider),
            "MixedTrust" => Some(Self::MixedTrust),
            _ => None,
        }
    }
}

/// Conversation phase (state-machine state, §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Thinking,
    AwaitingTool,
    AwaitingApproval,
    AwaitingWakeup,
    AwaitingReconnect,
    AwaitingTrustDecision,
    TrustRevoked,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Thinking => "thinking",
            Phase::AwaitingTool => "awaiting_tool",
            Phase::AwaitingApproval => "awaiting_approval",
            Phase::AwaitingWakeup => "awaiting_wakeup",
            Phase::AwaitingReconnect => "awaiting_reconnect",
            Phase::AwaitingTrustDecision => "awaiting_trust_decision",
            Phase::TrustRevoked => "trust_revoked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "thinking" => Some(Self::Thinking),
            "awaiting_tool" => Some(Self::AwaitingTool),
            "awaiting_approval" => Some(Self::AwaitingApproval),
            "awaiting_wakeup" => Some(Self::AwaitingWakeup),
            "awaiting_reconnect" => Some(Self::AwaitingReconnect),
            "awaiting_trust_decision" => Some(Self::AwaitingTrustDecision),
            "trust_revoked" => Some(Self::TrustRevoked),
            _ => None,
        }
    }
}

/// Modality (§2.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modality {
    Text,
    Voice,
}

impl Modality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Modality::Text => "Text",
            Modality::Voice => "Voice",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Text" => Some(Self::Text),
            "Voice" => Some(Self::Voice),
            _ => None,
        }
    }
}

/// A `state_conversations` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRow {
    pub conversation_id: ConversationId,
    pub kind: ConversationKind,
    pub last_seq: EventSeq,
    pub phase: Phase,
    pub controller_id: Option<String>,
    pub trust_class: String, // free text; crate::principal::TrustLevel is the typed shape
    pub snapshot_blob: Option<Vec<u8>>,
    pub snapshot_seq: Option<EventSeq>,
    pub lease_owner: Option<String>,
    pub lease_expires: Option<i64>,
    pub modality: Modality,
}

/// Simple repository for `state_conversations`.
pub struct ConversationStore<'db> {
    db: &'db Database,
}

impl<'db> ConversationStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn upsert(&self, row: &ConversationRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_conversations \
                 (conversation_id, kind, last_seq, phase, controller_id, trust_class, \
                  snapshot_blob, snapshot_seq, lease_owner, lease_expires, modality) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                 ON CONFLICT(conversation_id) DO UPDATE SET \
                    kind = excluded.kind, \
                    last_seq = excluded.last_seq, \
                    phase = excluded.phase, \
                    controller_id = excluded.controller_id, \
                    trust_class = excluded.trust_class, \
                    snapshot_blob = excluded.snapshot_blob, \
                    snapshot_seq = excluded.snapshot_seq, \
                    lease_owner = excluded.lease_owner, \
                    lease_expires = excluded.lease_expires, \
                    modality = excluded.modality",
                params![
                    row.conversation_id.as_str(),
                    row.kind.as_str(),
                    row.last_seq.0,
                    row.phase.as_str(),
                    row.controller_id,
                    row.trust_class,
                    row.snapshot_blob,
                    row.snapshot_seq.map(|s| s.0),
                    row.lease_owner,
                    row.lease_expires,
                    row.modality.as_str(),
                ],
            )?;
            Ok(())
        })
    }

    pub fn get(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<ConversationRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT kind, last_seq, phase, controller_id, trust_class, \
                            snapshot_blob, snapshot_seq, lease_owner, lease_expires, modality \
                     FROM state_conversations WHERE conversation_id = ?1",
                    params![conversation_id.as_str()],
                    |row| {
                        let kind_str: String = row.get(0)?;
                        let last_seq: i64 = row.get(1)?;
                        let phase_str: String = row.get(2)?;
                        let controller_id: Option<String> = row.get(3)?;
                        let trust_class: String = row.get(4)?;
                        let snapshot_blob: Option<Vec<u8>> = row.get(5)?;
                        let snapshot_seq: Option<i64> = row.get(6)?;
                        let lease_owner: Option<String> = row.get(7)?;
                        let lease_expires: Option<i64> = row.get(8)?;
                        let modality_str: String = row.get(9)?;
                        Ok((
                            kind_str,
                            last_seq,
                            phase_str,
                            controller_id,
                            trust_class,
                            snapshot_blob,
                            snapshot_seq,
                            lease_owner,
                            lease_expires,
                            modality_str,
                        ))
                    },
                )
                .ok();
            Ok(got.map(
                |(
                    kind_str,
                    last_seq,
                    phase_str,
                    controller_id,
                    trust_class,
                    snapshot_blob,
                    snapshot_seq,
                    lease_owner,
                    lease_expires,
                    modality_str,
                )| ConversationRow {
                    conversation_id: conversation_id.clone(),
                    kind: ConversationKind::parse(&kind_str)
                        .unwrap_or(ConversationKind::ControllerDM),
                    last_seq: EventSeq(last_seq),
                    phase: Phase::parse(&phase_str).unwrap_or(Phase::Idle),
                    controller_id,
                    trust_class,
                    snapshot_blob,
                    snapshot_seq: snapshot_seq.map(EventSeq),
                    lease_owner,
                    lease_expires,
                    modality: Modality::parse(&modality_str).unwrap_or(Modality::Text),
                },
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn upsert_and_get_roundtrip() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        let row = ConversationRow {
            conversation_id: ConversationId::from("c1"),
            kind: ConversationKind::ControllerDM,
            last_seq: EventSeq(0),
            phase: Phase::Idle,
            controller_id: Some("p1".into()),
            trust_class: "Controller".into(),
            snapshot_blob: None,
            snapshot_seq: None,
            lease_owner: None,
            lease_expires: None,
            modality: Modality::Text,
        };
        store.upsert(&row).unwrap();
        let got = store.get(&row.conversation_id).unwrap().unwrap();
        assert_eq!(got.kind, ConversationKind::ControllerDM);
        assert_eq!(got.phase, Phase::Idle);
        assert_eq!(got.modality, Modality::Text);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        let mut row = ConversationRow {
            conversation_id: ConversationId::from("c2"),
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
        };
        store.upsert(&row).unwrap();
        row.phase = Phase::AwaitingApproval;
        row.last_seq = EventSeq(17);
        store.upsert(&row).unwrap();
        let got = store.get(&row.conversation_id).unwrap().unwrap();
        assert_eq!(got.phase, Phase::AwaitingApproval);
        assert_eq!(got.last_seq, EventSeq(17));
    }
}
