//! `EventRetentionSweeper` — purges `state_events` rows past the
//! global `history_retention_days` window.
//!
//! Mirrors `LogRetentionSweeper` and `RoutineRunRetentionSweeper` in
//! shape: a long-running tokio task that wakes every `interval`,
//! reads the operator-configured `RetentionPolicy` from the DB,
//! deletes events older than `now - retention`, and exits cleanly on
//! a stop signal.
//!
//! Subtleties:
//!   * Conversations whose *every* event is past the cutoff have
//!     their `state_conversations` row deleted too — no orphan
//!     conversations sitting around with empty event histories.
//!   * Pinned conversations are exempt: pinning is the operator's
//!     explicit "this is precious, keep it" signal. The sweep
//!     leaves their events untouched regardless of age.
//!   * Ephemeral conversations are handled by `EphemeralSweeper`
//!     using their own TTL clock; this sweeper never touches them.
//!
//! 2026-04-29.

use crate::db::{Database, DbError};
use rusqlite::params;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default sweep cadence — once every two hours. Event-log writes
/// dominate the disk footprint long-term, but the chosen retention
/// is in days, so a sub-hour cadence offers no real benefit.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// Outcome of one sweep pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventSweepReport {
    pub events_deleted: usize,
    pub conversations_deleted: usize,
}

/// Run a single retention pass against `state_events`. Pure-ish —
/// caller supplies `now_unix` so tests pin time deterministically.
///
/// Pinned conversations (`state_conversations.is_pinned = 1`) are
/// excluded entirely. Ephemeral ones are excluded via `is_ephemeral
/// = 0` so the dedicated `EphemeralSweeper` owns them.
pub fn sweep_once(
    db: &Database,
    now_unix: i64,
    retention_secs: i64,
) -> Result<EventSweepReport, DbError> {
    let cutoff = now_unix.saturating_sub(retention_secs).max(0);
    let mut events_deleted = 0usize;
    let mut conversations_deleted = 0usize;

    db.with_conn(|c| {
        // 1. Delete events past the cutoff in non-pinned, non-ephemeral
        //    conversations.
        let n_events = c.execute(
            "DELETE FROM state_events \
             WHERE committed_at < ?1 \
               AND conversation_id IN ( \
                   SELECT conversation_id FROM state_conversations \
                   WHERE COALESCE(is_pinned, 0) = 0 \
                     AND COALESCE(is_ephemeral, 0) = 0 \
               )",
            params![cutoff],
        )?;
        events_deleted = n_events;

        // 2. Delete conversation rows that have no remaining events
        //    (also excluding pinned + ephemeral). The "no events
        //    AND last_activity_at older than cutoff" guard catches
        //    conversations that never accrued events but were
        //    abandoned anyway.
        let n_convs = c.execute(
            "DELETE FROM state_conversations \
             WHERE COALESCE(is_pinned, 0) = 0 \
               AND COALESCE(is_ephemeral, 0) = 0 \
               AND last_activity_at < ?1 \
               AND conversation_id NOT IN ( \
                   SELECT DISTINCT conversation_id FROM state_events \
               )",
            params![cutoff],
        )?;
        conversations_deleted = n_convs;
        Ok(())
    })?;

    if events_deleted > 0 || conversations_deleted > 0 {
        debug!(
            events = events_deleted,
            conversations = conversations_deleted,
            cutoff_unix = cutoff,
            "event retention sweep"
        );
    }
    Ok(EventSweepReport {
        events_deleted,
        conversations_deleted,
    })
}

#[derive(Clone)]
pub struct EventRetentionSweeper {
    db: Database,
    interval: Duration,
    /// `Some(d)` pins retention regardless of operator policy
    /// (tests). `None` means "load `RetentionPolicy` on each tick" —
    /// the production path.
    static_retention: Option<Duration>,
    kick: Arc<Notify>,
}

impl EventRetentionSweeper {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            interval: DEFAULT_SWEEP_INTERVAL,
            static_retention: None,
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn with_config(db: Database, interval: Duration, retention: Duration) -> Self {
        Self {
            db,
            interval,
            static_retention: Some(retention),
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            retention = match self.static_retention {
                Some(d) => format!("static:{}s", d.as_secs()),
                None => "policy".into(),
            },
            "event retention sweeper running"
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("event retention sweeper stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "event retention sweep failed; will retry next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<EventSweepReport, DbError> {
        let now_unix = chrono::Utc::now().timestamp();
        let retention_secs = match self.static_retention {
            Some(d) => d.as_secs() as i64,
            None => {
                let policy = crate::retention::RetentionPolicy::load(&self.db)?;
                if policy.is_infinite() {
                    return Ok(EventSweepReport::default());
                }
                policy.days as i64 * 86_400
            }
        };
        sweep_once(&self.db, now_unix, retention_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use crate::db::DbConfig;
    use crate::events::{EventKind, EventLog, EventRecord};
    use crate::ids::{ConversationId, EventSeq};
    use crate::migrations::MigrationRunner;
    use serde::Serialize;

    #[derive(Serialize)]
    struct P {
        text: String,
    }

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_conv(
        db: &Database,
        id: &str,
        last_activity_at: i64,
        is_pinned: bool,
        is_ephemeral: bool,
    ) -> ConversationId {
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
                display_name_source: "auto".into(),
                is_pinned,
                is_ephemeral,
                ephemeral_expires_at: None,
                last_activity_at,
            })
            .unwrap();
        cid
    }

    fn seed_event(db: &Database, cid: &ConversationId, seq: i64, ts: i64) {
        let ev = EventRecord {
            conversation_id: cid.clone(),
            seq: EventSeq(seq),
            kind: EventKind::UserMsg,
            payload: rmp_serde::to_vec(&P { text: "x".into() }).unwrap(),
            committed_at: ts,
            actor: None,
        };
        EventLog::new(db).append(&ev).unwrap();
    }

    fn count_events(db: &Database) -> i64 {
        db.with_conn(|c| {
            let v: i64 = c
                .query_row("SELECT COUNT(*) FROM state_events", [], |r| r.get(0))
                .unwrap();
            Ok(v)
        })
        .unwrap()
    }

    fn count_convs(db: &Database) -> i64 {
        db.with_conn(|c| {
            let v: i64 = c
                .query_row("SELECT COUNT(*) FROM state_conversations", [], |r| r.get(0))
                .unwrap();
            Ok(v)
        })
        .unwrap()
    }

    #[test]
    fn sweep_deletes_events_past_retention_window() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c1", 1000, false, false);
        seed_event(&db, &cid, 1, 100); // old
        seed_event(&db, &cid, 2, 200); // old
        seed_event(&db, &cid, 3, 1500); // recent
        // now=2000, retention=500 → cutoff=1500. 100 + 200 dropped;
        // 1500 stays (NOT strictly less).
        let r = sweep_once(&db, 2000, 500).unwrap();
        assert_eq!(r.events_deleted, 2);
        assert_eq!(count_events(&db), 1);
    }

    #[test]
    fn sweep_preserves_pinned_conversations() {
        let db = fresh_db();
        let pinned = seed_conv(&db, "pin", 100, true, false);
        let normal = seed_conv(&db, "norm", 100, false, false);
        seed_event(&db, &pinned, 1, 100);
        seed_event(&db, &normal, 1, 100);
        // now=10_000, retention=500 → cutoff=9500. Both events
        // are old enough to delete in principle.
        let r = sweep_once(&db, 10_000, 500).unwrap();
        // Only the normal conversation's event is deleted.
        assert_eq!(r.events_deleted, 1);
        assert_eq!(count_events(&db), 1);
        // The pinned conversation row is also preserved.
        assert!(ConversationStore::new(&db).get(&pinned).unwrap().is_some());
        // The empty normal conversation gets GC'd too.
        assert!(ConversationStore::new(&db).get(&normal).unwrap().is_none());
    }

    #[test]
    fn sweep_skips_ephemeral_conversations() {
        let db = fresh_db();
        let eph = seed_conv(&db, "eph", 100, false, true);
        seed_event(&db, &eph, 1, 100);
        let r = sweep_once(&db, 10_000, 500).unwrap();
        assert_eq!(r.events_deleted, 0);
        assert_eq!(count_events(&db), 1);
        // Ephemeral conv NOT deleted by this sweeper —
        // EphemeralSweeper owns that lifecycle.
        assert_eq!(count_convs(&db), 1);
    }

    #[test]
    fn empty_conversation_with_old_activity_gets_gcd() {
        let db = fresh_db();
        seed_conv(&db, "ghost", 50, false, false); // no events; last_activity_at far past cutoff
        let r = sweep_once(&db, 10_000, 500).unwrap();
        assert_eq!(r.events_deleted, 0);
        assert_eq!(r.conversations_deleted, 1);
        assert_eq!(count_convs(&db), 0);
    }

    #[test]
    fn sweep_with_zero_retention_drops_everything_unpinned() {
        let db = fresh_db();
        let normal = seed_conv(&db, "norm", 100, false, false);
        seed_event(&db, &normal, 1, 100);
        // retention=0 → cutoff = now (saturating). Nothing strictly
        // less than now=100? cutoff=100, ts<100 false. So nothing
        // deleted with these timestamps. Use larger now.
        let _ = normal;
        let r = sweep_once(&db, 100_000, 0).unwrap();
        assert_eq!(r.events_deleted, 1);
    }

    #[test]
    fn sweep_is_idempotent_on_repeat() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c", 100, false, false);
        seed_event(&db, &cid, 1, 100);
        let r1 = sweep_once(&db, 10_000, 500).unwrap();
        let r2 = sweep_once(&db, 10_000, 500).unwrap();
        assert_eq!(r1.events_deleted, 1);
        assert_eq!(r2.events_deleted, 0);
    }
}
