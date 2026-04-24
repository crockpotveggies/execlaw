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

    /// Fetch up to `limit` outbox rows that are ready to deliver — `pending`
    /// status and either no `next_attempt_at` or `next_attempt_at <= now`.
    /// Ordered by id (FIFO).
    pub fn ready_pending(&self, now_ts: i64, limit: i64) -> Result<Vec<OutboxRow>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, idempotency_key, conversation_id, effect_kind, payload, status, \
                        attempts, next_attempt_at, last_error, enqueued_seq \
                 FROM state_outbox \
                 WHERE status = 'pending' \
                   AND (next_attempt_at IS NULL OR next_attempt_at <= ?1) \
                 ORDER BY id ASC \
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![now_ts, limit], row_to_outbox)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Atomically mark a row `in_flight` if and only if it is currently
    /// `pending`. Returns true if the caller now owns dispatch.
    ///
    /// This is the leasing primitive that prevents two drain-loop
    /// iterations from dispatching the same row.
    pub fn claim(&self, id: i64) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_outbox SET status = 'in_flight' WHERE id = ?1 AND status = 'pending'",
                params![id],
            )?;
            Ok(n == 1)
        })
    }

    /// Mark a claimed row as successfully delivered.
    pub fn mark_delivered(&self, id: i64) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_outbox SET status = 'delivered', last_error = NULL \
                 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    /// Record a failed attempt. Bumps `attempts`; if under the retry
    /// budget, sets status back to `pending` with `next_attempt_at` for
    /// the backoff schedule. If over budget, status → `dead_letter`.
    pub fn record_failure(
        &self,
        id: i64,
        error: &str,
        retry_budget_max: u32,
        backoff_secs: i64,
    ) -> Result<bool, DbError> {
        // Returns true if retrying, false if moved to dead_letter.
        self.db.transaction(|tx| {
            let attempts: i64 = tx.query_row(
                "SELECT attempts FROM state_outbox WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            let new_attempts = attempts + 1;
            if (new_attempts as u32) >= retry_budget_max {
                tx.execute(
                    "UPDATE state_outbox SET status = 'dead_letter', last_error = ?1, \
                         attempts = ?2 WHERE id = ?3",
                    params![error, new_attempts, id],
                )?;
                Ok(false)
            } else {
                let next_attempt_at = chrono::Utc::now().timestamp() + backoff_secs;
                tx.execute(
                    "UPDATE state_outbox SET status = 'pending', last_error = ?1, \
                         attempts = ?2, next_attempt_at = ?3 WHERE id = ?4",
                    params![error, new_attempts, next_attempt_at, id],
                )?;
                Ok(true)
            }
        })
    }

    /// Count rows currently in dead_letter. Useful for alerting.
    pub fn dead_letter_count(&self) -> Result<i64, DbError> {
        self.db.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM state_outbox WHERE status = 'dead_letter'",
                [],
                |r| r.get(0),
            )?;
            Ok(n)
        })
    }
}

fn row_to_outbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    let id: i64 = row.get(0)?;
    let idempotency_key: String = row.get(1)?;
    let conversation_id: String = row.get(2)?;
    let effect_kind: String = row.get(3)?;
    let payload: Vec<u8> = row.get(4)?;
    let status: String = row.get(5)?;
    let attempts: i64 = row.get(6)?;
    let next_attempt_at: Option<i64> = row.get(7)?;
    let last_error: Option<String> = row.get(8)?;
    let enqueued_seq: i64 = row.get(9)?;
    Ok(OutboxRow {
        id: Some(id),
        idempotency_key: IdempotencyKey::from_string(idempotency_key),
        conversation_id: ConversationId::from(conversation_id),
        effect_kind,
        payload,
        status: OutboxStatus::parse(&status).unwrap_or(OutboxStatus::Pending),
        attempts,
        next_attempt_at,
        last_error,
        enqueued_seq: EventSeq(enqueued_seq),
    })
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

    fn mk_row(cid: &ConversationId, ord: u32) -> OutboxRow {
        OutboxRow {
            id: None,
            idempotency_key: IdempotencyKey::mint(cid, crate::ids::TurnSeq(1), ord),
            conversation_id: cid.clone(),
            effect_kind: "test.effect".into(),
            payload: b"p".to_vec(),
            status: OutboxStatus::Pending,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            enqueued_seq: EventSeq(1),
        }
    }

    /// `claim` is the leasing primitive — two concurrent drain loops
    /// must not both win a claim on the same row.
    #[test]
    fn claim_is_mutually_exclusive() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let id = store.enqueue(&mk_row(&ConversationId::from("c"), 0)).unwrap();
        assert!(store.claim(id).unwrap(), "first claim should succeed");
        assert!(!store.claim(id).unwrap(), "second claim must fail");
    }

    /// `record_failure` bumps attempts and sets next_attempt_at while
    /// under budget, then transitions to dead_letter when over.
    #[test]
    fn record_failure_retries_under_budget_then_dead_letters() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let id = store.enqueue(&mk_row(&ConversationId::from("c"), 0)).unwrap();

        // Under budget: status returns to pending, row has next_attempt_at set.
        let retrying = store.record_failure(id, "boom", 3, 60).unwrap();
        assert!(retrying);
        let (status, attempts, next): (String, i64, Option<i64>) = db
            .with_conn(|c| {
                let v = c
                    .query_row(
                        "SELECT status, attempts, next_attempt_at FROM state_outbox WHERE id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .unwrap();
                Ok(v)
            })
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(attempts, 1);
        assert!(next.is_some() && next.unwrap() > 0);

        // Two more failures push attempts past budget → dead_letter.
        let _ = store.record_failure(id, "boom", 3, 60).unwrap();
        let retrying3 = store.record_failure(id, "boom", 3, 60).unwrap();
        assert!(!retrying3, "third failure past budget must dead-letter");
        assert_eq!(store.dead_letter_count().unwrap(), 1);
    }

    /// `ready_pending` must skip rows whose `next_attempt_at` is in
    /// the future — otherwise backoff would have no effect.
    #[test]
    fn ready_pending_skips_rows_with_future_next_attempt() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let cid = ConversationId::from("c");

        // Row 1: due now.
        let _ = store.enqueue(&OutboxRow {
            next_attempt_at: Some(100),
            ..mk_row(&cid, 0)
        }).unwrap();
        // Row 2: due far in the future.
        let _ = store.enqueue(&OutboxRow {
            next_attempt_at: Some(10_000_000),
            ..mk_row(&cid, 1)
        }).unwrap();
        // Row 3: no schedule — always ready.
        let _ = store.enqueue(&mk_row(&cid, 2)).unwrap();

        let ready = store.ready_pending(500, 100).unwrap();
        assert_eq!(ready.len(), 2, "only rows 1 and 3 are due at ts=500");
        for r in &ready {
            assert!(r.next_attempt_at.is_none() || r.next_attempt_at.unwrap() <= 500);
        }
    }

    /// `mark_delivered` clears `last_error` and sets status=delivered.
    #[test]
    fn mark_delivered_clears_error() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let id = store.enqueue(&mk_row(&ConversationId::from("c"), 0)).unwrap();
        // First record a failure so last_error is non-null.
        let _ = store.record_failure(id, "transient", 5, 1).unwrap();
        // Claim is required in production but the SQL works without; drive
        // via mark_delivered directly.
        store.mark_delivered(id).unwrap();

        let (status, err): (String, Option<String>) = db
            .with_conn(|c| {
                let v = c
                    .query_row(
                        "SELECT status, last_error FROM state_outbox WHERE id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                Ok(v)
            })
            .unwrap();
        assert_eq!(status, "delivered");
        assert!(err.is_none(), "mark_delivered must clear last_error");
    }

    /// OutboxStatus parse covers every variant — forward-compat guard.
    #[test]
    fn outbox_status_parse_roundtrips_all_variants() {
        for v in [
            OutboxStatus::Pending,
            OutboxStatus::InFlight,
            OutboxStatus::Delivered,
            OutboxStatus::Failed,
            OutboxStatus::DeadLetter,
        ] {
            assert_eq!(OutboxStatus::parse(v.as_str()), Some(v));
        }
        assert_eq!(OutboxStatus::parse("bogus"), None);
    }
}
