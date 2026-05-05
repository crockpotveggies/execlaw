//! `EphemeralSweeper` — purges incognito-thread events once their
//! TTL elapses (§2.6).
//!
//! Incognito threads persist their events while the conversation is
//! live (so crash recovery, idempotency replay, and the tool-pairing
//! invariant all keep working) but the rows are NOT meant to stick
//! around forever. Once `ephemeral_expires_at <= now`, this sweeper:
//!
//! 1. DELETEs every `state_events` row for the conversation,
//! 2. UPDATEs `state_conversations` so `last_seq = 0`, the snapshot
//!    blob is cleared, and `ephemeral_expires_at = NULL` (so the
//!    same row isn't picked up again on the next tick),
//! 3. Leaves `is_ephemeral = 1` set as a forensic marker — reports
//!    can show "N incognito threads existed but their content was
//!    purged."
//!
//! Outbox rows referencing the conversation are intentionally left
//! alone: an in-flight `transport.send` effect should still deliver
//! even if its parent conversation has aged out of incognito; the
//! alternative (leaking suppressed sends) is worse.
//!
//! The sweeper runs in a tokio task and respects a stop signal so
//! it can drain cleanly on shutdown. The pure-function entry point
//! [`sweep_once`] is what tests exercise — the loop is a thin
//! tokio wrapper.

use crate::conversation::ConversationStore;
use crate::db::{Database, DbError};
use crate::ids::ConversationId;
use rusqlite::params;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default sweep cadence (matches MIGRATION_PLAN §2.6 — "every ~5 min").
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Outcome of one sweep pass — useful for tests, metrics, and
/// run-loop logging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub conversations_purged: usize,
    pub events_deleted: usize,
}

/// Run a single sweep pass against `now` (unix seconds). Pure-ish: the
/// only side effects are SQL DELETE + UPDATE inside one transaction
/// per conversation, so a crash mid-sweep can't half-purge a thread.
///
/// Returns the totals so callers can emit metrics / logs.
pub fn sweep_once(db: &Database, now: i64) -> Result<SweepReport, DbError> {
    let convs = ConversationStore::new(db);
    let due: Vec<ConversationId> = convs.list_expired_ephemeral(now)?;

    let mut report = SweepReport::default();
    for cid in due {
        let deleted = purge_one(db, &cid)?;
        report.conversations_purged += 1;
        report.events_deleted += deleted;
        debug!(
            conversation_id = %cid,
            events_deleted = deleted,
            "ephemeral conversation purged"
        );
    }
    if report.conversations_purged > 0 {
        info!(
            conversations = report.conversations_purged,
            events = report.events_deleted,
            "ephemeral sweep completed"
        );
    }
    Ok(report)
}

fn purge_one(db: &Database, cid: &ConversationId) -> Result<usize, DbError> {
    db.transaction(|tx| {
        let deleted = tx.execute(
            "DELETE FROM state_events WHERE conversation_id = ?1",
            params![cid.as_str()],
        )?;
        // Reset FSM-side state so a stale snapshot can't resurrect
        // anything; clear ephemeral_expires_at so we don't re-sweep
        // this row on the next tick.
        tx.execute(
            "UPDATE state_conversations \
             SET last_seq = 0, \
                 snapshot_blob = NULL, \
                 snapshot_seq = NULL, \
                 ephemeral_expires_at = NULL \
             WHERE conversation_id = ?1",
            params![cid.as_str()],
        )?;
        Ok(deleted)
    })
}

/// Long-running sweeper task. Cheap to clone (the inner state is just
/// `Database` + an `Arc<Notify>`).
#[derive(Clone)]
pub struct EphemeralSweeper {
    db: Database,
    interval: Duration,
    /// Wake the run loop early — used by tests to avoid `sleep`s and
    /// by ops to force a sweep on demand without waiting for the next
    /// tick.
    kick: Arc<Notify>,
}

impl EphemeralSweeper {
    pub fn new(db: Database) -> Self {
        Self::with_interval(db, DEFAULT_SWEEP_INTERVAL)
    }

    pub fn with_interval(db: Database, interval: Duration) -> Self {
        Self {
            db,
            interval,
            kick: Arc::new(Notify::new()),
        }
    }

    /// Force the run loop to sweep now instead of waiting for the next
    /// tick. Coalesces — multiple kicks while the loop is busy collapse
    /// into one extra sweep.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Drive the sweep loop until `stop` is notified. Each iteration
    /// reads `now` from the system clock so test fakes that pin time
    /// at the DB layer are out of scope here — the unit-test surface
    /// is [`sweep_once`].
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "ephemeral sweeper running"
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("ephemeral sweeper stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "ephemeral sweep failed; will retry on next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<SweepReport, DbError> {
        let now = chrono::Utc::now().timestamp();
        sweep_once(&self.db, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{ConversationKind, ConversationRow, Modality, Phase};
    use crate::db::DbConfig;
    use crate::events::{EventKind, EventLog, EventRecord};
    use crate::ids::EventSeq;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn make_row(id: &str) -> ConversationRow {
        ConversationRow {
            conversation_id: ConversationId::from(id),
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
        }
    }

    fn append_n_events(db: &Database, cid: &ConversationId, n: i64) {
        let log = EventLog::new(db);
        for i in 1..=n {
            let ev = EventRecord::new(
                cid.clone(),
                EventSeq(i),
                EventKind::UserMsg,
                &serde_json::json!({"i": i}),
                None,
            )
            .unwrap();
            log.append(&ev).unwrap();
        }
    }

    fn count_events(db: &Database, cid: &ConversationId) -> i64 {
        db.with_conn(|c| {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM state_events WHERE conversation_id = ?1",
                    params![cid.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            Ok(n)
        })
        .unwrap()
    }

    #[test]
    fn sweep_purges_expired_incognito_events_and_resets_last_seq() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);

        // Two incognito threads — one expired, one not.
        let expired_id = ConversationId::from("expired");
        let mut expired = make_row("expired");
        expired.last_seq = EventSeq(3);
        // Pretend the snapshot was non-trivial — sweeper should clear it.
        expired.snapshot_blob = Some(vec![1, 2, 3]);
        expired.snapshot_seq = Some(EventSeq(3));
        convs.upsert(&expired).unwrap();
        convs.mark_ephemeral(&expired_id, Some(50)).unwrap();
        append_n_events(&db, &expired_id, 3);

        let live_id = ConversationId::from("live");
        let mut live = make_row("live");
        live.last_seq = EventSeq(2);
        convs.upsert(&live).unwrap();
        convs.mark_ephemeral(&live_id, Some(10_000)).unwrap();
        append_n_events(&db, &live_id, 2);

        // Sweep at now=100: expired (50) goes, live (10_000) stays.
        let report = sweep_once(&db, 100).unwrap();
        assert_eq!(report.conversations_purged, 1);
        assert_eq!(report.events_deleted, 3);

        // Expired conversation: events gone, last_seq zeroed, snapshot cleared,
        // is_ephemeral marker preserved, expires_at cleared.
        assert_eq!(count_events(&db, &expired_id), 0);
        let row = convs.get(&expired_id).unwrap().unwrap();
        assert_eq!(row.last_seq, EventSeq(0));
        assert!(row.snapshot_blob.is_none());
        assert!(row.snapshot_seq.is_none());
        assert!(row.is_ephemeral, "forensic marker preserved");
        assert_eq!(row.ephemeral_expires_at, None, "no re-sweep next tick");

        // Live conversation untouched.
        assert_eq!(count_events(&db, &live_id), 2);
        let live_row = convs.get(&live_id).unwrap().unwrap();
        assert_eq!(live_row.last_seq, EventSeq(2));
    }

    #[test]
    fn sweep_is_idempotent_when_run_twice() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);
        let cid = ConversationId::from("c");
        convs.upsert(&make_row("c")).unwrap();
        convs.mark_ephemeral(&cid, Some(50)).unwrap();
        append_n_events(&db, &cid, 5);

        let r1 = sweep_once(&db, 100).unwrap();
        assert_eq!(r1.conversations_purged, 1);
        assert_eq!(r1.events_deleted, 5);

        let r2 = sweep_once(&db, 100).unwrap();
        assert_eq!(r2.conversations_purged, 0);
        assert_eq!(r2.events_deleted, 0);
    }

    /// Non-ephemeral conversations must NEVER be touched by the sweeper.
    #[test]
    fn sweep_ignores_non_ephemeral_conversations() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);
        let cid = ConversationId::from("regular");
        convs.upsert(&make_row("regular")).unwrap();
        append_n_events(&db, &cid, 4);

        sweep_once(&db, 9_999_999).unwrap();
        assert_eq!(count_events(&db, &cid), 4);
    }

    /// `expires_at IS NULL` rows (never marked, or already-purged) must
    /// not appear in the candidate list.
    #[test]
    fn sweep_skips_null_expires_at() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);
        let cid = ConversationId::from("nullexp");
        convs.upsert(&make_row("nullexp")).unwrap();
        // is_ephemeral=1 but no expiry set — not a sweep target.
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_conversations SET is_ephemeral = 1 WHERE conversation_id = ?1",
                params![cid.as_str()],
            )?;
            Ok(())
        })
        .unwrap();
        append_n_events(&db, &cid, 1);

        let r = sweep_once(&db, 9_999_999).unwrap();
        assert_eq!(r.conversations_purged, 0);
        assert_eq!(count_events(&db, &cid), 1);
    }

    /// Boundary: `expires_at == now` MUST be purged (the SQL uses `<=`).
    /// Documents the boundary so a future tweak doesn't silently flip it.
    #[test]
    fn sweep_purges_at_exact_expiry_boundary() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);
        let cid = ConversationId::from("edge");
        convs.upsert(&make_row("edge")).unwrap();
        convs.mark_ephemeral(&cid, Some(100)).unwrap();
        append_n_events(&db, &cid, 1);

        let r = sweep_once(&db, 100).unwrap();
        assert_eq!(r.conversations_purged, 1);
    }

    /// Adversarial: many expired threads sweep in O(N) without
    /// crossing convoluted SQL paths. Just a sanity check on the
    /// per-conversation transaction loop.
    #[test]
    fn sweep_handles_many_expired_in_one_pass() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);
        for i in 0..50 {
            let id = format!("c{i}");
            let cid = ConversationId::from(id.as_str());
            convs.upsert(&make_row(&id)).unwrap();
            convs.mark_ephemeral(&cid, Some(50)).unwrap();
            append_n_events(&db, &cid, 2);
        }

        let r = sweep_once(&db, 100).unwrap();
        assert_eq!(r.conversations_purged, 50);
        assert_eq!(r.events_deleted, 100);
    }

    #[tokio::test]
    async fn run_loop_sweeps_then_stops_on_signal() {
        let db = fresh_db();
        let convs = ConversationStore::new(&db);
        let cid = ConversationId::from("c-loop");
        convs.upsert(&make_row("c-loop")).unwrap();
        // Expired far in the past so any system-clock sweep targets it.
        convs.mark_ephemeral(&cid, Some(1)).unwrap();
        append_n_events(&db, &cid, 2);

        let sweeper = EphemeralSweeper::with_interval(db.clone(), Duration::from_millis(10));
        let stop = Arc::new(Notify::new());

        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });

        // Force one sweep without waiting for the interval, then stop.
        sweeper.kick();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.notify_one();
        handle.await.unwrap();

        // The sweeper drains once on stop too, so events must be gone.
        assert_eq!(count_events(&db, &cid), 0);
    }
}
