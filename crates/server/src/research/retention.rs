//! Research-job retention sweeper (C6).
//!
//! Mirrors `LogRetentionSweeper` / `RoutineRunRetentionSweeper`:
//! a long-running tokio task that wakes every `interval`, computes
//! the cutoff from the live `RetentionPolicy`, deletes terminal
//! rows past the cutoff, and best-effort removes the on-disk
//! workspace directories the runner provisioned.
//!
//! Active rows are never swept regardless of age — a job that's
//! been in `Planning` for hours might be stuck on a slow LLM, not
//! abandoned. The supervisor's `auto_cancel_after_idle_secs` cap
//! is the right knob for that, not retention.
//!
//! The two-phase delete (SQL transaction → workspace `remove_dir_all`)
//! is intentional: SQL atomicity guarantees the DB side; the
//! filesystem cleanup runs OUTSIDE the transaction so a slow
//! recursive remove can't hold the SQLite write-lock open.
//!
//! 2026-04-29.

use crate::research::workspace::{ResearchWorkspace, WorkspaceError};
use execlaw_core::Database;
use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::{ResearchError, ResearchJobStore};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default sweep cadence. Research jobs accrete slowly (minutes-to-
/// hours each); an hourly tick is plenty.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResearchRetentionReport {
    pub rows_deleted: usize,
    pub workspace_dirs_removed: usize,
    pub workspace_failures: usize,
}

/// Run a single retention pass. Pure-ish — caller supplies
/// `now_unix` so tests pin time deterministically. The workspace
/// handle drives the filesystem half of the sweep; pass a
/// `tempfile::TempDir`-rooted workspace in tests so the assertions
/// don't accidentally touch a real `~/.execlaw/research/` dir.
pub fn sweep_once(
    db: &Database,
    workspace: &ResearchWorkspace,
    now_unix: i64,
    retention_secs: i64,
) -> Result<ResearchRetentionReport, ResearchError> {
    let cutoff = now_unix.saturating_sub(retention_secs);
    let store = ResearchJobStore::new(db);
    let purged = store.purge_terminal_older_than(cutoff)?;
    if purged.is_empty() {
        return Ok(ResearchRetentionReport::default());
    }
    let mut workspace_dirs_removed = 0usize;
    let mut workspace_failures = 0usize;
    for (job_id, workspace_path) in &purged {
        match purge_dir_for_row(workspace, job_id, workspace_path.as_deref()) {
            Ok(true) => workspace_dirs_removed += 1,
            Ok(false) => {} // dir didn't exist; not a failure
            Err(e) => {
                workspace_failures += 1;
                warn!(
                    job_id = job_id.as_str(),
                    error = %e,
                    "research-retention: workspace dir purge failed; row already deleted",
                );
            }
        }
    }
    let report = ResearchRetentionReport {
        rows_deleted: purged.len(),
        workspace_dirs_removed,
        workspace_failures,
    };
    debug!(
        rows = report.rows_deleted,
        dirs = report.workspace_dirs_removed,
        failures = report.workspace_failures,
        cutoff_unix = cutoff,
        "research-retention sweep",
    );
    Ok(report)
}

/// Decide which directory to rm-rf for a given row. Prefers the row's
/// stored `workspace_path` (covers the case where the operator moved
/// the workspace root mid-life); falls back to `workspace.purge(id)`
/// against the supplied root for rows that were created before
/// `workspace_path` was added to the row.
fn purge_dir_for_row(
    workspace: &ResearchWorkspace,
    job_id: &ResearchJobId,
    stored_path: Option<&str>,
) -> Result<bool, WorkspaceError> {
    if let Some(path_str) = stored_path {
        let path = PathBuf::from(path_str);
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
            return Ok(true);
        }
        return Ok(false);
    }
    // No stored path — fall back to the workspace's default layout.
    let default_path = workspace.root().join(job_id.as_str());
    if default_path.exists() {
        workspace.purge(job_id)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Long-running sweeper actor. Constructed in `cmd_serve`; runs
/// for the lifetime of the process.
#[derive(Clone)]
pub struct ResearchRetentionSweeper {
    db: Database,
    workspace: ResearchWorkspace,
    interval: Duration,
    /// `Some(d)` pins retention regardless of operator policy
    /// (tests + the `with_config` constructor). `None` means "load
    /// the live `RetentionPolicy` on each tick" — the production path.
    static_retention: Option<Duration>,
    kick: Arc<Notify>,
}

impl ResearchRetentionSweeper {
    pub fn new(db: Database, workspace: ResearchWorkspace) -> Self {
        Self {
            db,
            workspace,
            interval: DEFAULT_SWEEP_INTERVAL,
            static_retention: None,
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn with_config(
        db: Database,
        workspace: ResearchWorkspace,
        interval: Duration,
        retention: Duration,
    ) -> Self {
        Self {
            db,
            workspace,
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
            "research-retention sweeper running",
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("research-retention stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "research-retention sweep failed; retry next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<ResearchRetentionReport, ResearchError> {
        let now_unix = chrono::Utc::now().timestamp();
        let retention_secs = match self.static_retention {
            Some(d) => d.as_secs() as i64,
            None => {
                let policy = execlaw_core::retention::RetentionPolicy::load(&self.db)
                    .map_err(ResearchError::Db)?;
                if policy.is_infinite() {
                    return Ok(ResearchRetentionReport::default());
                }
                policy.days as i64 * 86_400
            }
        };
        sweep_once(&self.db, &self.workspace, now_unix, retention_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::{ConversationId, EventSeq};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::research::{ResearchJobStatus, ResearchJobStore};

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

    fn seed_terminal_with_workspace(
        db: &Database,
        workspace_root: &std::path::Path,
        cid: &ConversationId,
        finished_at: i64,
        status: ResearchJobStatus,
    ) -> (ResearchJobId, std::path::PathBuf) {
        let store = ResearchJobStore::new(db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, cid, "q", "Controller", None, finished_at - 100)
            .unwrap();
        store
            .claim_next_pending("card-x", finished_at - 50)
            .unwrap();
        if matches!(status, ResearchJobStatus::Failed) {
            store
                .finish(&id, status, Some("boom"), None, finished_at)
                .unwrap();
        } else {
            store
                .finish(&id, status, None, Some("att-x"), finished_at)
                .unwrap();
        }
        let dir = workspace_root.join(id.as_str());
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("plan.json"), "{}").unwrap();
        store
            .set_workspace_path(&id, &dir.to_string_lossy(), finished_at)
            .unwrap();
        (id, dir)
    }

    #[test]
    fn sweep_deletes_old_terminal_rows_and_purges_their_workspace_dirs() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c");
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        // Old terminal job.
        let (old_id, old_dir) =
            seed_terminal_with_workspace(&db, tmp.path(), &cid, 100, ResearchJobStatus::Complete);
        // Recent terminal job.
        let (recent_id, recent_dir) = seed_terminal_with_workspace(
            &db,
            tmp.path(),
            &cid,
            999_999,
            ResearchJobStatus::Complete,
        );
        // Active job (Pending) — never gets swept regardless of age.
        let store = ResearchJobStore::new(&db);
        let active_id = ResearchJobId::new();
        store
            .insert_pending(&active_id, &cid, "active", "Controller", None, 50)
            .unwrap();
        // retention=500 → cutoff=999_500. Old (finished_at=100) gets
        // swept; recent (999_999) and active (no finished_at) stay.
        let report = sweep_once(&db, &workspace, 1_000_000, 500).unwrap();
        assert_eq!(report.rows_deleted, 1);
        assert_eq!(report.workspace_dirs_removed, 1);
        assert_eq!(report.workspace_failures, 0);
        // DB side: old row gone, others present.
        assert!(store.get(&old_id).unwrap().is_none());
        assert!(store.get(&recent_id).unwrap().is_some());
        assert!(store.get(&active_id).unwrap().is_some());
        // Workspace side: old dir gone, recent dir intact.
        assert!(!old_dir.exists());
        assert!(recent_dir.exists());
    }

    #[test]
    fn sweep_skips_active_rows_even_when_old_for_long_time() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c");
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 0)
            .unwrap();
        // Pretend the row is years old. finished_at is NULL because
        // it's still Pending; the sweeper's `finished_at IS NOT NULL`
        // predicate must skip it.
        let report = sweep_once(&db, &workspace, 999_999_999, 1).unwrap();
        assert_eq!(report.rows_deleted, 0);
        assert!(store.get(&id).unwrap().is_some());
    }

    #[test]
    fn sweep_tolerates_missing_workspace_dir() {
        // Seed a terminal row with a workspace_path that points
        // somewhere the dir was already deleted (e.g. operator
        // wiped the dir manually). The sweeper logs but does not
        // count it as a failure — `Ok(false)` from the helper.
        let db = fresh_db();
        let cid = seed_conv(&db, "c");
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 0)
            .unwrap();
        store.claim_next_pending("c", 50).unwrap();
        store
            .finish(&id, ResearchJobStatus::Complete, None, Some("att"), 100)
            .unwrap();
        // workspace_path stored, but the dir doesn't exist on disk.
        store
            .set_workspace_path(
                &id,
                tmp.path().join("never-created").to_string_lossy().as_ref(),
                100,
            )
            .unwrap();
        let report = sweep_once(&db, &workspace, 1_000_000, 1).unwrap();
        assert_eq!(report.rows_deleted, 1);
        assert_eq!(report.workspace_dirs_removed, 0);
        assert_eq!(report.workspace_failures, 0);
        assert!(store.get(&id).unwrap().is_none());
    }

    #[test]
    fn sweep_is_idempotent() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c");
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let _ =
            seed_terminal_with_workspace(&db, tmp.path(), &cid, 100, ResearchJobStatus::Complete);
        let _ = sweep_once(&db, &workspace, 1_000_000, 500).unwrap();
        let again = sweep_once(&db, &workspace, 1_000_000, 500).unwrap();
        assert_eq!(again.rows_deleted, 0);
        assert_eq!(again.workspace_dirs_removed, 0);
    }

    #[test]
    fn sweep_now_skips_when_policy_is_infinite() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c");
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        // Stamp old terminal row.
        let (id, _) =
            seed_terminal_with_workspace(&db, tmp.path(), &cid, 100, ResearchJobStatus::Complete);
        // Set retention to 0 (infinite) on the global policy.
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET history_retention_days = 0 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let sweeper = ResearchRetentionSweeper::new(db.clone(), workspace);
        let report = sweeper.sweep_now().unwrap();
        assert_eq!(report.rows_deleted, 0);
        // Row still present.
        let store = ResearchJobStore::new(&db);
        assert!(store.get(&id).unwrap().is_some());
    }

    #[tokio::test]
    async fn run_loop_drains_on_stop_signal() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c");
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let _ = seed_terminal_with_workspace(&db, tmp.path(), &cid, 1, ResearchJobStatus::Complete);
        let sweeper = ResearchRetentionSweeper::with_config(
            db.clone(),
            workspace,
            Duration::from_millis(20),
            Duration::from_secs(1),
        );
        let stop = Arc::new(Notify::new());
        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });
        sweeper.kick();
        tokio::time::sleep(Duration::from_millis(80)).await;
        stop.notify_one();
        handle.await.unwrap();
        // Terminal row was swept.
        let store = ResearchJobStore::new(&db);
        assert_eq!(store.list_all().unwrap().len(), 0);
    }
}
