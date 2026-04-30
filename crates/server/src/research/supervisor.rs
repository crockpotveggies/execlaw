//! Supervisor — picks up `Pending` rows and spawns runners.
//!
//! One tokio task that wakes every `TICK_INTERVAL` (5s by default,
//! short enough that operator-initiated jobs feel snappy without
//! hammering the DB). Per tick:
//!
//!   1. Atomically claim the next `Pending` row (mints a card_id
//!      and flips status to `Planning` in one transaction so two
//!      supervisors can't grab the same job).
//!   2. Resolve an inference client via [`crate::inference_resolver
//!      ::InferenceResolver`].
//!   3. Spawn a per-job tokio task driving `runner::run_job`.
//!
//! Failures inside the runner are isolated — the supervisor keeps
//! ticking. The runner is responsible for marking the row `Failed`
//! and emitting `CardClosed{Failed}` on its own.
//!
//! 2026-04-29.

use crate::inference_resolver::InferenceResolver;
use crate::research::runner::{JobRunCtx, run_job};
use crate::research::workspace::ResearchWorkspace;
use execlaw_core::Database;
use execlaw_core::backends::BackendPurpose;
use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::ResearchJobStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::warn;
use uuid::Uuid;

const TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Construction inputs for the supervisor. Cheap to clone (every
/// field is `Arc`-backed or trivially copyable).
#[derive(Clone)]
pub struct ResearchSupervisor {
    pub db: Database,
    pub inference: Arc<InferenceResolver>,
    pub workspace: ResearchWorkspace,
    pub model: String,
}

impl ResearchSupervisor {
    pub fn new(
        db: Database,
        inference: Arc<InferenceResolver>,
        workspace: ResearchWorkspace,
        model: String,
    ) -> Self {
        Self {
            db,
            inference,
            workspace,
            model,
        }
    }

    /// Run the supervisor loop until `stop` fires. Owned by
    /// `cmd_serve`; the `Notify` handle is the same one the other
    /// sweepers consume so a SIGTERM drains everything together.
    pub async fn run(self, stop: Arc<Notify>) {
        tracing::info!(
            interval_secs = TICK_INTERVAL.as_secs(),
            "research supervisor running"
        );
        loop {
            tokio::select! {
                _ = stop.notified() => {
                    tracing::info!("research supervisor stop signal received");
                    break;
                }
                _ = tokio::time::sleep(TICK_INTERVAL) => {}
            }
            if let Err(e) = self.tick_once().await {
                warn!("research supervisor tick failed: {e}");
            }
        }
    }

    /// Single tick. Public so tests can drive it directly.
    pub async fn tick_once(&self) -> Result<(), String> {
        // Drain every pending row in one tick — small jobs shouldn't
        // wait `TICK_INTERVAL` in line behind the previous one. We
        // cap at a sane batch (8) so a Pending flood can't starve
        // other tokio tasks; the next tick picks up the rest.
        for _ in 0..8 {
            let card_id = Uuid::new_v4().to_string();
            let claimed = {
                let db = self.db.clone();
                let card_id = card_id.clone();
                tokio::task::spawn_blocking(move || {
                    let now = chrono::Utc::now().timestamp();
                    ResearchJobStore::new(&db).claim_next_pending(&card_id, now)
                })
                .await
                .map_err(|e| format!("join: {e}"))?
                .map_err(|e| format!("claim: {e}"))?
            };
            let row = match claimed {
                Some(r) => r,
                None => break, // no more pending rows this tick
            };
            self.spawn_runner_for(row.id);
        }
        Ok(())
    }

    fn spawn_runner_for(&self, job_id: ResearchJobId) {
        let db = self.db.clone();
        let workspace = self.workspace.clone();
        let model = self.model.clone();
        let inference_resolver = self.inference.clone();
        tokio::spawn(async move {
            let inference = inference_resolver
                .resolve(&db, BackendPurpose::Standard)
                .map(|c| (c, model));
            let ctx = JobRunCtx {
                db,
                job_id: job_id.clone(),
                workspace,
                inference,
            };
            if let Err(e) = run_job(ctx).await {
                tracing::warn!(
                    job_id = job_id.as_str(),
                    error = %e,
                    "research runner exited with error",
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::{ConversationId, EventSeq, ResearchJobId};
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

    /// With no inference backend wired, `tick_once` still claims the
    /// `Pending` row and drives it to `Failed` through the runner's
    /// `NoInference` path. The runner runs in a spawned task, so we
    /// poll the row a few times before giving up.
    #[tokio::test]
    async fn tick_claims_pending_and_drives_to_failed_when_no_inference() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c1");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&db)
            .insert_pending(&id, &cid, "what's new?", "Controller", None, 100)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let resolver = Arc::new(InferenceResolver::new(None));
        let sup = ResearchSupervisor::new(db.clone(), resolver, workspace, "test-model".into());
        sup.tick_once().await.unwrap();

        // Poll the row up to ~1s for the runner task to land.
        let mut row = None;
        for _ in 0..40 {
            let r = ResearchJobStore::new(&db).get(&id).unwrap().unwrap();
            if r.status.is_terminal() {
                row = Some(r);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let row = row.expect("runner task should have driven the row to a terminal state");
        assert_eq!(row.status, ResearchJobStatus::Failed);
        assert!(row.error.is_some());
    }

    /// Two `Pending` rows in one tick — both get drained without a
    /// second tick, up to the per-tick batch cap.
    #[tokio::test]
    async fn tick_drains_multiple_pending_rows_in_one_pass() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c1");
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = ResearchJobId::new();
            ResearchJobStore::new(&db)
                .insert_pending(
                    &id,
                    &cid,
                    &format!("query {i}"),
                    "Controller",
                    None,
                    100 + i,
                )
                .unwrap();
            ids.push(id);
        }
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let resolver = Arc::new(InferenceResolver::new(None));
        let sup = ResearchSupervisor::new(db.clone(), resolver, workspace, "test-model".into());
        sup.tick_once().await.unwrap();

        // After the tick, every row should have been claimed (status
        // != Pending). The actual runner outcome lands a beat later
        // — that's what the prior test exercises.
        for id in &ids {
            let row = ResearchJobStore::new(&db).get(id).unwrap().unwrap();
            assert_ne!(row.status, ResearchJobStatus::Pending);
        }
    }
}
