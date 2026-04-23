//! Outbox + inbox data-model helpers (§2.4).
//!
//! Actual relay behavior — backoff schedule, dispatch to transport plugins,
//! dead-letter handling — lives in the sibling `execlaw-outbox` crate. This
//! module only owns the DB shape and basic CRUD.

use crate::db::{Database, DbError};
use crate::ids::{ConversationId, EventSeq, IdempotencyKey};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboxStatus {
    Pending,
    InFlight,
    Delivered,
    Failed,
    DeadLetter,
}

impl OutboxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutboxStatus::Pending => "pending",
            OutboxStatus::InFlight => "in_flight",
            OutboxStatus::Delivered => "delivered",
            OutboxStatus::Failed => "failed",
            OutboxStatus::DeadLetter => "dead_letter",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_flight" => Some(Self::InFlight),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            "dead_letter" => Some(Self::DeadLetter),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRow {
    pub id: Option<i64>, // set after INSERT
    pub idempotency_key: IdempotencyKey,
    pub conversation_id: ConversationId,
    pub effect_kind: String, // e.g. "transport.send", "schedule.wakeup"
    pub payload: Vec<u8>,    // MessagePack
    pub status: OutboxStatus,
    pub attempts: i64,
    pub next_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub enqueued_seq: EventSeq,
}

pub struct OutboxStore<'db> {
    db: &'db Database,
}

impl<'db> OutboxStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a new outbox row, returning the assigned rowid.
    pub fn enqueue(&self, row: &OutboxRow) -> Result<i64, DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_outbox \
                 (idempotency_key, conversation_id, effect_kind, payload, status, \
                  attempts, next_attempt_at, last_error, enqueued_seq) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.idempotency_key.as_str(),
                    row.conversation_id.as_str(),
                    row.effect_kind,
                    row.payload,
                    row.status.as_str(),
                    row.attempts,
                    row.next_attempt_at,
                    row.last_error,
                    row.enqueued_seq.0,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn mark_status(
        &self,
        id: i64,
        status: OutboxStatus,
        last_error: Option<&str>,
        next_attempt_at: Option<i64>,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_outbox SET status = ?1, last_error = ?2, \
                 next_attempt_at = ?3, attempts = attempts + 1 WHERE id = ?4",
                params![status.as_str(), last_error, next_attempt_at, id],
            )?;
            Ok(())
        })
    }

    /// Try to record a delivery in the inbox. Returns `true` if this was the
    /// first time we saw this idempotency key (caller should proceed with
    /// side effect), `false` if it was already recorded (caller should skip).
    pub fn inbox_record_if_new(&self, key: &IdempotencyKey) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let changed = c.execute(
                "INSERT OR IGNORE INTO state_inbox(idempotency_key, received_at) VALUES (?1, ?2)",
                params![key.as_str(), chrono::Utc::now().timestamp()],
            )?;
            Ok(changed > 0)
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
    fn enqueue_and_dedup_idempotency_key() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let cid = ConversationId::from("c1");
        let key = IdempotencyKey::mint(&cid, crate::ids::TurnSeq(1), 0);
        let row = OutboxRow {
            id: None,
            idempotency_key: key.clone(),
            conversation_id: cid.clone(),
            effect_kind: "transport.send".into(),
            payload: b"payload".to_vec(),
            status: OutboxStatus::Pending,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            enqueued_seq: EventSeq(1),
        };
        let id = store.enqueue(&row).unwrap();
        assert!(id > 0);

        // Inserting the same idempotency_key must fail (UNIQUE constraint).
        let dup = store.enqueue(&row);
        assert!(dup.is_err(), "duplicate idempotency key must be rejected");
    }

    #[test]
    fn inbox_dedup_only_records_once() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let key = IdempotencyKey::from_string("k1");
        assert!(store.inbox_record_if_new(&key).unwrap());
        assert!(!store.inbox_record_if_new(&key).unwrap());
    }
}

