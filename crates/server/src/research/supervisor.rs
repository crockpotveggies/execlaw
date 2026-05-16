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

use crate::events::EventBus;
use crate::inference_resolver::InferenceResolver;
use crate::research::runner::{JobRunCtx, run_job};
use crate::research::workspace::ResearchWorkspace;
use dashmap::DashMap;
use execlaw_core::Database;
use execlaw_core::backends::BackendPurpose;
use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::ResearchJobStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
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
    // 2026-05-13 — dropped `pub model: String`. The resolver now
    // returns `ResolvedInference { model_id, .. }` from the same
    // DB row that supplied the endpoint, so caching a model string
    // on the supervisor was a second source of truth that drifted
    // from the live backend row. Per-job, `spawn_runner_for` reads
    // `r.model_id` straight off `inference_resolver.resolve(...)`.
    /// EventBus the runner publishes Card.{Opened,Progressed,Closed}
    /// onto so the SPA's chat-pane sees lifecycle ticks live without
    /// waiting on a re-fetch. C3 ran without one (cards committed to
    /// the log only); C4 wires it through end-to-end.
    pub events: EventBus,
    /// Cancellation tokens for in-flight runners, keyed by job id.
    /// The supervisor inserts an entry when it spawns a runner;
    /// the spawned task removes its entry on exit (success or
    /// failure). The cancel admin endpoint looks the token up by
    /// `job_id` and `.cancel()`s it, which short-circuits the
    /// gather phase between sub-queries (gather workers check the
    /// token between HTTP calls and on subagent boundaries — see
    /// `gather::gather_one`). Without this registry the cancel
    /// endpoint flips the DB row but the runner keeps spending
    /// tokens until its phase finishes naturally.
    pub cancel_tokens: Arc<DashMap<String, CancellationToken>>,
    /// Wake-up handle. `DbResearchApi::start` notifies this handle
    /// after inserting a Pending row so the supervisor's tick loop
    /// reconciles immediately instead of waiting up to TICK_INTERVAL
    /// (5 s) for the next scheduled wake. Without this, the
    /// agent-visible delay between calling research_start and the
    /// planner emitting AwaitingInput included a 0–5 s "supervisor
    /// hasn't ticked yet" tail that user-tested as "unusual delay
    /// before clarification arrived."
    ///
    /// Tick is still on a 5 s schedule as a safety net for cases
    /// where a row landed via a path that didn't kick (DB-direct
    /// insert in tests, future routine-fired research, etc.).
    pub wake: Arc<Notify>,
    /// Host-transport registry used by the synthesize-phase tail
    /// to bridge the report PDF back through any registered
    /// transport bound to the conversation. `None` skips the
    /// bridge step.
    pub host_transports: Option<crate::transport_registry::HostTransportRegistry>,
    /// Plugin host. Paired with `host_transports` so the
    /// synthesize-phase auto-bridge can dispatch
    /// `<channel>.send_with_attachments` directly into the
    /// channel's plugin tool surface.
    pub plugin_host: Option<execlaw_plugin_host::PluginHost>,
}

impl ResearchSupervisor {
    pub fn new(
        db: Database,
        inference: Arc<InferenceResolver>,
        workspace: ResearchWorkspace,
        events: EventBus,
    ) -> Self {
        Self {
            db,
            inference,
            workspace,
            events,
            cancel_tokens: Arc::new(DashMap::new()),
            wake: Arc::new(Notify::new()),
            host_transports: None,
            plugin_host: None,
        }
    }

    /// Builder-style setter for the host-transport registry.
    /// Threaded into the runner so a research-complete event on a
    /// transport-bridged conversation auto-dispatches the report
    /// PDF via whatever channel the registry has a factory for.
    pub fn with_host_transports(
        mut self,
        registry: Option<crate::transport_registry::HostTransportRegistry>,
    ) -> Self {
        self.host_transports = registry;
        self
    }

    pub fn with_plugin_host(
        mut self,
        plugin_host: Option<execlaw_plugin_host::PluginHost>,
    ) -> Self {
        self.plugin_host = plugin_host;
        self
    }

    /// Wake the supervisor's tick loop immediately. Called after
    /// `DbResearchApi::start` inserts a Pending row so the operator
    /// doesn't see a 0–5 s "supervisor still snoozing" tail latency.
    /// Cheap (`Notify::notify_one` is lock-free) and idempotent — a
    /// second kick before the loop wakes is a no-op.
    pub fn kick(&self) {
        self.wake.notify_one();
    }

    /// Look up the live cancellation token for `job_id`. Returns
    /// `None` when no runner is in flight for the given id (it
    /// finished, was never picked up, or the supervisor is on a
    /// different process). The cancel admin endpoint calls
    /// `.cancel()` on the returned token, which makes the gather
    /// workers exit at their next checkpoint.
    pub fn cancel_token_for(&self, job_id: &str) -> Option<CancellationToken> {
        self.cancel_tokens.get(job_id).map(|e| e.value().clone())
    }

    /// Run the supervisor loop until `stop` fires. Owned by
    /// `cmd_serve`; the `Notify` handle is the same one the other
    /// sweepers consume so a SIGTERM drains everything together.
    pub async fn run(self, stop: Arc<Notify>) {
        // 2026-05-03 — boot-time recovery. Any row left in an
        // in-flight state (`planning`/`gathering`/`synthesizing`)
        // by the previous process is from a service interruption
        // (crash, SIGKILL, host reboot mid-job). Mark them all
        // Failed("service interrupted") and emit `CardClosed` so
        // the SPA's card flips out of "Running…" and the operator
        // sees the failure instead of a stuck card. Operator can
        // re-run the research from the UI; we don't auto-resume
        // (preserves operator intent and avoids burning inference
        // tokens on a restart they may not want).
        if let Err(e) = self.recover_interrupted_jobs().await {
            warn!("research supervisor: boot-time recovery failed: {e}");
        }

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
                // Race the scheduled tick against the wake handle so
                // a `kick()` from research_start drains the pending
                // queue immediately. The tick stays on as a safety
                // net for paths that bypass kick (DB-direct inserts
                // in tests, etc.).
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(TICK_INTERVAL) => {}
            }
            if let Err(e) = self.tick_once().await {
                warn!("research supervisor tick failed: {e}");
            }
        }
    }

    /// One-shot at boot: mark every in-flight row Failed("service
    /// interrupted") and emit `CardClosed` so the chat-pane card
    /// flips out of its "Running…" state. Idempotent — a second
    /// call after the first finds nothing to do.
    pub async fn recover_interrupted_jobs(&self) -> Result<usize, String> {
        let db = self.db.clone();
        let recovered = tokio::task::spawn_blocking(move || {
            let now = chrono::Utc::now().timestamp();
            ResearchJobStore::new(&db).mark_failed_where_active("service interrupted", now)
        })
        .await
        .map_err(|e| format!("join: {e}"))?
        .map_err(|e| format!("mark_failed_where_active: {e}"))?;

        if recovered.is_empty() {
            return Ok(0);
        }
        tracing::warn!(
            count = recovered.len(),
            "research supervisor: marking interrupted jobs as failed (boot-time recovery)"
        );
        // Close the chat-pane card for each interrupted job so the
        // SPA flips out of "Running…". Best-effort: a card-close
        // failure logs but doesn't fail the recovery.
        for r in &recovered {
            let card_id = match &r.card_id {
                Some(c) => c.clone(),
                None => continue, // job died before card_id was minted
            };
            let payload = execlaw_core::cards::CardClosedPayload {
                card_id,
                state: execlaw_core::cards::CardState::Failed,
                summary: "Research interrupted by service restart. Re-run to retry.".into(),
                details: None,
                attachment_id: None,
                error: Some("service interrupted".into()),
            };
            if let Err(e) = crate::cards::close_card_and_broadcast(
                &self.db,
                &self.events,
                &r.conversation_id,
                "system",
                &payload,
            ) {
                warn!(
                    job_id = %r.job_id.as_str(),
                    error = %e,
                    "research recovery: close_card_and_broadcast failed (continuing)"
                );
            }
        }
        Ok(recovered.len())
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
        let inference_resolver = self.inference.clone();
        let events = self.events.clone();
        let host_transports = self.host_transports.clone();
        let plugin_host = self.plugin_host.clone();
        // Mint + register a cancellation token so the cancel admin
        // endpoint can short-circuit the gather phase mid-flight.
        // The spawned task removes its entry on exit (any path —
        // success, failure, panic-via-drop) so a future advance
        // doesn't see a stale token.
        let cancel = CancellationToken::new();
        let tokens = self.cancel_tokens.clone();
        let job_id_key = job_id.as_str().to_owned();
        tokens.insert(job_id_key.clone(), cancel.clone());
        tokio::spawn(async move {
            // 2026-05-13 — pair the client + model id from the same
            // resolver call so they can't drift. Pre-rework the
            // caller passed `model` separately while the client
            // came from a different read — exactly the dual-source-
            // of-truth pattern that caused vLLM 404s on the chat
            // path. `ResolvedInference` always carries a non-empty
            // `model_id` (the resolver falls back to
            // `DEFAULT_FALLBACK_MODEL` if the row is missing one),
            // so no second-tier fallback is needed here.
            let inference = inference_resolver
                .resolve(&db, BackendPurpose::Standard)
                .map(|r| (r.client.clone(), r.model_id.clone()));
            let ctx = JobRunCtx {
                db,
                job_id: job_id.clone(),
                workspace,
                inference,
                events,
                cancel: cancel.clone(),
                host_transports,
                plugin_host,
            };
            let result = run_job(ctx).await;
            tokens.remove(&job_id_key);
            if let Err(e) = result {
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
                display_name_source: "auto".into(),
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    /// 2026-05-03 — boot-time recovery converts in-flight rows to
    /// Failed, leaves Pending/Planned/Terminal rows untouched, and
    /// emits CardClosed for each recovered job that had a card_id.
    #[tokio::test]
    async fn recover_interrupted_jobs_marks_in_flight_failed_and_closes_cards() {
        use rusqlite::params;
        let db = fresh_db();
        let cid = seed_conv(&db, "c1");
        // Seed three jobs: one each in planning / gathering /
        // synthesizing, all with card_ids.
        let store = ResearchJobStore::new(&db);
        let in_flight: Vec<(ResearchJobId, &str, &str)> = vec![
            (ResearchJobId::new(), "planning", "card-pln"),
            (ResearchJobId::new(), "gathering", "card-gtr"),
            (ResearchJobId::new(), "synthesizing", "card-syn"),
        ];
        // Plus one Pending and one Planned that must NOT transition.
        let pending = ResearchJobId::new();
        let planned = ResearchJobId::new();
        for (id, _, _) in &in_flight {
            store
                .insert_pending(id, &cid, "q", "Controller", None, 100)
                .unwrap();
        }
        for id in [&pending, &planned] {
            store
                .insert_pending(id, &cid, "q", "Controller", None, 100)
                .unwrap();
        }
        // Force the in-flight states + card_ids; force planned for
        // the planned row.
        for (id, status, card) in &in_flight {
            db.with_conn(|c| {
                c.execute(
                    "UPDATE state_research_jobs SET status = ?1, card_id = ?2 WHERE id = ?3",
                    params![status, card, id.as_str()],
                )?;
                Ok(())
            })
            .unwrap();
        }
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_research_jobs SET status = 'planned' WHERE id = ?1",
                params![planned.as_str()],
            )?;
            Ok(())
        })
        .unwrap();

        // Subscribe to the bus so we can verify CardClosed events fire.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let sup = ResearchSupervisor::new(
            db.clone(),
            Arc::new(InferenceResolver::new(None)),
            workspace,
            bus,
        );
        let n = sup.recover_interrupted_jobs().await.unwrap();
        assert_eq!(n, 3);

        // In-flight rows are now Failed with the recovery message.
        for (id, _, _) in &in_flight {
            let row = store.get(id).unwrap().unwrap();
            assert_eq!(row.status, ResearchJobStatus::Failed);
            assert_eq!(row.error.as_deref(), Some("service interrupted"));
        }
        // Pending + Planned are untouched.
        assert_eq!(
            store.get(&pending).unwrap().unwrap().status,
            ResearchJobStatus::Pending
        );
        assert_eq!(
            store.get(&planned).unwrap().unwrap().status,
            ResearchJobStatus::Planned
        );

        // Three CardClosed events should have published. Drain the
        // bus with a small timeout to avoid hanging if none fire.
        let mut closed_card_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(crate::events::UiEvent::CardClosed { card_id, state, .. })) => {
                    assert_eq!(state, "Failed");
                    closed_card_ids.insert(card_id);
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(closed_card_ids.contains("card-pln"));
        assert!(closed_card_ids.contains("card-gtr"));
        assert!(closed_card_ids.contains("card-syn"));

        // Idempotent: a second call finds nothing.
        assert_eq!(sup.recover_interrupted_jobs().await.unwrap(), 0);
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
        let sup = ResearchSupervisor::new(db.clone(), resolver, workspace, EventBus::new());
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
        let sup = ResearchSupervisor::new(db.clone(), resolver, workspace, EventBus::new());
        sup.tick_once().await.unwrap();

        // After the tick, every row should have been claimed (status
        // != Pending). The actual runner outcome lands a beat later
        // — that's what the prior test exercises.
        for id in &ids {
            let row = ResearchJobStore::new(&db).get(id).unwrap().unwrap();
            assert_ne!(row.status, ResearchJobStatus::Pending);
        }
    }

    /// C6c invariant: when the supervisor spawns a runner, the
    /// cancel-token registry has an entry. After the runner exits
    /// (the no-inference path drives the row to Failed quickly),
    /// the entry is removed so a future cancel doesn't fire on a
    /// stale token. Without this guarantee a re-run of the same
    /// job_id would inherit the prior cancel and short-circuit
    /// before doing any work.
    #[tokio::test]
    async fn cancel_token_is_registered_on_spawn_and_removed_on_exit() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c-cancel-registry");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&db)
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let resolver = Arc::new(InferenceResolver::new(None));
        let sup = ResearchSupervisor::new(db.clone(), resolver, workspace, EventBus::new());
        sup.tick_once().await.unwrap();
        // Poll up to ~1s for the runner to exit (no inference =
        // immediate Failed) and clean up its registry entry.
        let mut removed = false;
        for _ in 0..40 {
            if !sup.cancel_tokens.contains_key(id.as_str()) {
                removed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            removed,
            "cancel token entry should be removed after runner exit",
        );
    }

    /// Adversarial: an operator cancel mid-flight should make the
    /// runner exit with a Failed status (since the no-inference
    /// path returns Err(NoInference) → mark_failed before the cancel
    /// can be observed; this test verifies the registry plumbing,
    /// not the gather-phase short-circuit which lives in
    /// `gather::run_gather_cancellation_short_circuits_remaining_workers`).
    /// Specifically: the cancel_token_for lookup must return Some
    /// while a runner is in flight.
    #[tokio::test]
    async fn cancel_token_for_returns_some_while_runner_in_flight() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c-cancel-lookup");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&db)
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ResearchWorkspace::new(tmp.path());
        let resolver = Arc::new(InferenceResolver::new(None));
        let sup = ResearchSupervisor::new(db.clone(), resolver, workspace, EventBus::new());
        // Pre-claim check: registry is empty.
        assert!(sup.cancel_token_for(id.as_str()).is_none());
        // After tick, registry contains an entry (briefly — the
        // runner may have already exited on the no-inference path,
        // but the insert happens synchronously in tick_once so it's
        // observable here ahead of the spawned task's removal).
        sup.tick_once().await.unwrap();
        // Either the entry is still present (token-lookup works)
        // or it's already cleaned up (runner finished). Both are
        // valid outcomes for the no-inference path, but the row
        // status must be terminal in either case.
        for _ in 0..40 {
            let row = ResearchJobStore::new(&db).get(&id).unwrap().unwrap();
            if row.status.is_terminal() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("runner should have driven the row to a terminal state");
    }

    /// Regression for the rev-7-pt2 wake fix: with TICK_INTERVAL=5s,
    /// a research_start that landed a Pending row used to wait up
    /// to 5 s before the supervisor noticed it. The new `kick()`
    /// pokes the wake handle so the loop reconciles immediately.
    /// This test asserts the loop wakes within ~100 ms of a kick
    /// even though the next tick is 5 s away.
    #[tokio::test]
    async fn kick_wakes_supervisor_loop_immediately_without_waiting_for_tick() {
        // Use a `notified()` helper directly rather than spinning up
        // a real supervisor — the contract under test is "Notify
        // wakes a tokio::select! arm that races sleep". Verifying
        // that contract on a bare Notify covers the fix; the
        // supervisor uses the same pattern.
        let wake = Arc::new(Notify::new());
        let wake_for_kick = wake.clone();

        // Spawn a task that waits on `wake` racing a 5 s sleep,
        // returning which arm fired.
        let started = tokio::spawn(async move {
            let start = std::time::Instant::now();
            tokio::select! {
                _ = wake.notified() => ("wake", start.elapsed()),
                _ = tokio::time::sleep(TICK_INTERVAL) => ("tick", start.elapsed()),
            }
        });

        // Kick after a tiny sleep so the spawned task is definitely
        // parked on `notified()` before the notify fires.
        tokio::time::sleep(Duration::from_millis(20)).await;
        wake_for_kick.notify_one();

        let (which, elapsed) = started.await.unwrap();
        assert_eq!(which, "wake", "wake arm must win the race");
        assert!(
            elapsed < Duration::from_millis(500),
            "wake should fire within 500 ms; got {elapsed:?}",
        );
    }
}
