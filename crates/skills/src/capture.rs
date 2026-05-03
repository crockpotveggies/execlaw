//! Phase C — auto-capture worker.
//!
//! Architecture:
//!
//! ```text
//!  chat handler           AutoCaptureSink         AutoCaptureWorker
//!  (turn complete) ──enqueue(conv,seq)──▶ mpsc ──▶ drains ──▶ pipeline
//! ```
//!
//! The chat handler calls `sink.enqueue(...)` at turn completion
//! (cheap, non-blocking — just a channel send). The worker runs as
//! a tokio task that pulls each request, replays the conversation's
//! event log up to `until_seq`, extracts the (tool_use, tool_result)
//! pairs since the last user message, runs the sanitizer, fires the
//! summarizer, and (if not in dry-run mode) writes the resulting
//! `Draft` skill via the SkillStore.
//!
//! The worker is opt-in: `SkillsConfig::auto_capture_enabled = false`
//! short-circuits the pipeline at the very top, so an idle operator
//! pays only the cost of one `SkillsConfig::get()` per turn.
//!
//! Failures inside the pipeline are logged at warn level but never
//! propagated upward — capture is best-effort and must not affect
//! the operator's chat latency or success.

use crate::model::{
    NewProposal, NewSkill, NewSkillVersion, ProposalKind, RegistrationKind, SkillError,
};
use crate::sanitizer::{sanitize_step, SanitizationReport, SanitizedStep};
use crate::scanner::Strictness;
use crate::store::SkillStore;
use crate::summarizer::{
    DraftSkillProposal, SkillSummarizer, SummarizerOutput, SummarizerPrompt,
};
use execlaw_core::events::{
    EventKind, EventLog, EventRecord, ToolResultPayload, ToolUsePayload,
};
use execlaw_core::ids::{ConversationId, EventSeq};
use execlaw_core::skills_config::{SkillsConfig, SkillsConfigStore};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What the chat handler enqueues at turn completion. `until_seq`
/// is the latest event seq committed for the conversation; the
/// worker replays everything `<= until_seq` and processes events
/// since the most recent `UserMsg`.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub conversation_id: ConversationId,
    pub until_seq: EventSeq,
    /// Opaque run id used for the skill's `source` and `authored_by`
    /// fields (so the operator can trace which turn produced the
    /// proposal). Pass any stable token — typically the assistant
    /// turn's seq stringified, or a uuid.
    pub run_id: String,
}

/// Cheap, clonable handle the chat handler uses. Wraps an
/// `mpsc::UnboundedSender`. When the worker isn't installed (e.g.
/// in tests), use [`AutoCaptureSink::noop`] to get a sink that
/// silently drops every enqueue.
#[derive(Clone)]
pub struct AutoCaptureSink {
    tx: Option<mpsc::UnboundedSender<CaptureRequest>>,
}

impl AutoCaptureSink {
    pub fn noop() -> Self {
        Self { tx: None }
    }

    /// Try to enqueue a capture request. Never blocks. Returns
    /// `false` when the worker isn't installed OR the channel is
    /// closed (worker died); the chat handler can ignore the result
    /// — auto-capture failure is intentionally non-fatal.
    pub fn enqueue(&self, req: CaptureRequest) -> bool {
        match &self.tx {
            None => false,
            Some(tx) => tx.send(req).is_ok(),
        }
    }
}

/// Outcome of one capture pipeline run. Returned by
/// [`AutoCaptureWorker::process_request`] for tests / telemetry;
/// the running task itself just logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// Worker is dormant — `auto_capture_enabled = false`.
    Disabled,
    /// Trajectory had fewer than `min_tool_calls` tool calls.
    BelowThreshold { tool_calls: usize, threshold: u32 },
    /// Trajectory had a failed tool call; we only summarize success.
    HadFailure,
    /// Summarizer judged this trajectory non-generalizable.
    Skipped { reason: String },
    /// `auto_capture_dry_run` was on; pipeline ran but no skill
    /// was written.
    DryRun { proposal: DraftSkillProposal },
    /// Skill written successfully.
    Created { name: String },
    /// Pipeline completed but the write was rejected by the
    /// scanner (last-line defense). Logged loudly because it means
    /// the sanitizer let something through.
    Blocked { name: String, reason: String },
    /// Skill name conflict (e.g. an admin-authored skill already
    /// owns the name). Worker logs and moves on.
    Conflict { name: String },
    /// An unexpected error during replay / parsing. Logged so the
    /// operator can investigate.
    Error { message: String },
}

/// The worker. Owns the receiver end of the capture channel + the
/// dependencies it needs to run the pipeline. Cheap to construct.
pub struct AutoCaptureWorker {
    db: execlaw_core::Database,
    skill_store: Arc<SkillStore>,
    summarizer: Arc<dyn SkillSummarizer>,
    /// Maximum tokens the summarizer is allowed to emit per call.
    /// Caps the body size before it ever reaches the scanner.
    pub summarizer_max_tokens: u32,
}

impl AutoCaptureWorker {
    pub fn new(
        db: execlaw_core::Database,
        skill_store: Arc<SkillStore>,
        summarizer: Arc<dyn SkillSummarizer>,
    ) -> Self {
        Self {
            db,
            skill_store,
            summarizer,
            summarizer_max_tokens: 1024,
        }
    }

    /// Construct a sink + spawn a tokio task that drains it.
    /// Returns the sink (clone-and-share to chat handlers) and the
    /// task's `JoinHandle` (the caller can `abort()` on shutdown).
    ///
    /// Panic isolation: each request runs inside its own
    /// `tokio::spawn`, so a panic anywhere in the pipeline (bad
    /// payload, summarizer client crash, scanner regex pathological
    /// input) only kills that one request — the worker loop keeps
    /// pulling subsequent requests. Audit fix 2026-05-03.
    pub fn spawn(self: Arc<Self>) -> (AutoCaptureSink, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<CaptureRequest>();
        let me = self.clone();
        let handle = tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let me_inner = me.clone();
                let req_inner = req.clone();
                let task = tokio::spawn(async move {
                    let outcome = me_inner.process_request(req_inner.clone()).await;
                    log_outcome(&req_inner, &outcome);
                });
                if let Err(e) = task.await {
                    if e.is_panic() {
                        tracing::error!(
                            conversation_id = %req.conversation_id.as_str(),
                            run_id = %req.run_id,
                            "auto-capture pipeline PANICKED; worker continues"
                        );
                    }
                }
            }
        });
        (AutoCaptureSink { tx: Some(tx) }, handle)
    }

    /// Run the full pipeline once for a single request. Public so
    /// integration tests can drive the worker without the channel.
    pub async fn process_request(&self, req: CaptureRequest) -> CaptureOutcome {
        // 1. Config gate.
        let cfg = match SkillsConfigStore::new(&self.db).get() {
            Ok(c) => c,
            Err(e) => return CaptureOutcome::Error { message: e.to_string() },
        };
        if !cfg.auto_capture_enabled && !cfg.auto_capture_dry_run {
            return CaptureOutcome::Disabled;
        }

        // 2. Replay events for the conversation.
        let log = EventLog::new(&self.db);
        let events = match log.replay_since(&req.conversation_id, EventSeq(0)) {
            Ok(v) => v,
            Err(e) => return CaptureOutcome::Error { message: e.to_string() },
        };

        // 3. Slice from the last UserMsg up to until_seq.
        let trajectory = extract_latest_trajectory(&events, req.until_seq);

        // 4. Threshold check.
        let tool_call_count = trajectory.iter().filter(|s| s.is_tool_use()).count();
        if (tool_call_count as u32) < cfg.auto_capture_min_tool_calls {
            return CaptureOutcome::BelowThreshold {
                tool_calls: tool_call_count,
                threshold: cfg.auto_capture_min_tool_calls,
            };
        }

        // 5. Pair (tool_use, tool_result) by ordinal + sanitize.
        let mut report = SanitizationReport::default();
        let (steps, had_failure) = pair_and_sanitize(&trajectory, &mut report);
        if had_failure {
            // We only learn from successful procedures; a failed
            // tool call signals the trajectory itself isn't a
            // good template.
            return CaptureOutcome::HadFailure;
        }
        if steps.is_empty() {
            return CaptureOutcome::BelowThreshold {
                tool_calls: 0,
                threshold: cfg.auto_capture_min_tool_calls,
            };
        }

        // 6. Build prompt + summarize.
        let user_intent = extract_user_intent(&events, req.until_seq).map(|s| {
            // Sanitize the user's own message too — operators have
            // been known to paste credentials into chat.
            let mut ir = SanitizationReport::default();
            crate::sanitizer::sanitize_step(
                0,
                "_user_intent",
                &serde_json::Value::String(s),
                &Ok(serde_json::Value::Null),
                &mut ir,
            )
            .args_json
            .as_str()
            .map(|x| x.to_string())
            .unwrap_or_default()
        });
        let prompt = SummarizerPrompt {
            steps,
            user_intent,
            max_tokens: self.summarizer_max_tokens,
        };
        let reply = match self.summarizer.summarize(prompt).await {
            Ok(r) => r,
            Err(e) => return CaptureOutcome::Error { message: e },
        };
        let proposal = match reply {
            SummarizerOutput::Skip { reason } => return CaptureOutcome::Skipped { reason },
            SummarizerOutput::Draft(p) => p,
        };

        // 7. Validate the proposal's name before doing anything
        // that might write.
        if let Err(e) = crate::model::validate_skill_name(&proposal.name) {
            return CaptureOutcome::Error {
                message: format!("model returned invalid skill name: {e}"),
            };
        }

        // 8. Dry-run short-circuit — Phase D.1 now persists the
        // proposal to `state_skill_proposals` for operator review
        // instead of dropping it to a tracing log. Operator can
        // approve via the Skills page Proposals tab; approval runs
        // the same SkillStore::create path as a non-dry-run write.
        if cfg.auto_capture_dry_run {
            let summary = format!(
                "auto-capture proposal from {} tool call(s) in conversation {}",
                tool_call_count,
                req.conversation_id.as_str()
            );
            let frontmatter = serde_json::json!({
                "name": proposal.name,
                "description": proposal.description,
                "tags": proposal.tags,
                "agent_proposed": true,
                "run_id": req.run_id,
            })
            .to_string();
            let now_ms = chrono::Utc::now().timestamp() * 1000;
            let new_p = NewProposal {
                kind: ProposalKind::NewSkill,
                target_skill_id: None,
                proposed_name: proposal.name.clone(),
                description: proposal.description.clone(),
                body_md: proposal.body_md.clone(),
                frontmatter_json: frontmatter,
                source_run_id: req.run_id.clone(),
                trajectory_summary: Some(summary),
                tool_calls_observed: tool_call_count as u32,
            };
            return match self.skill_store.submit_proposal(new_p, now_ms) {
                Ok(_) => CaptureOutcome::DryRun { proposal },
                Err(e) => CaptureOutcome::Error {
                    message: format!("failed to persist proposal: {e}"),
                },
            };
        }

        // 9. Write through the SkillStore. Strict scanner mode —
        // last line of defense if the sanitizer missed anything.
        let frontmatter = serde_json::json!({
            "name": proposal.name,
            "description": proposal.description,
            "tags": proposal.tags,
            "agent_proposed": true,
            "run_id": req.run_id,
        })
        .to_string();
        let new = NewSkill {
            name: proposal.name.clone(),
            source: format!("agent:{}", req.run_id),
            registration_kind: RegistrationKind::Authored,
            owning_plugin_id: None,
            initial_version: NewSkillVersion {
                description: proposal.description.clone(),
                body_md: proposal.body_md.clone(),
                frontmatter_json: frontmatter,
                authored_by: format!("agent:{}", req.run_id),
                promotion_notes: None,
            },
            resources: vec![],
        };
        let now_ms = chrono::Utc::now().timestamp() * 1000;
        match self.skill_store.create(new, Strictness::Strict, now_ms) {
            Ok(_) => CaptureOutcome::Created {
                name: proposal.name,
            },
            Err(SkillError::Blocked { findings, fields }) => CaptureOutcome::Blocked {
                name: proposal.name,
                reason: format!("scanner blocked: {findings} finding(s) in {fields:?}"),
            },
            Err(SkillError::AlreadyExists(_)) | Err(SkillError::Db(_)) => {
                // Treat all DB errors that look like name conflicts
                // as a soft conflict; bubble everything else up.
                CaptureOutcome::Conflict { name: proposal.name }
            }
            Err(e) => CaptureOutcome::Error { message: e.to_string() },
        }
    }

    pub fn store(&self) -> &Arc<SkillStore> {
        &self.skill_store
    }
    pub fn config(&self) -> Result<SkillsConfig, execlaw_core::DbError> {
        SkillsConfigStore::new(&self.db).get()
    }
}

#[derive(Debug, Clone)]
enum TrajectoryEntry {
    ToolUse { ordinal: u32, name: String, args: JsonValue },
    ToolResult { ordinal: u32, outcome: Result<JsonValue, String> },
}

impl TrajectoryEntry {
    fn is_tool_use(&self) -> bool {
        matches!(self, TrajectoryEntry::ToolUse { .. })
    }
}

fn extract_latest_trajectory(events: &[EventRecord], until: EventSeq) -> Vec<TrajectoryEntry> {
    // Find the last UserMsg at or before `until`, then walk forward
    // collecting tool_use + tool_result up to `until`.
    let mut last_user_idx = None;
    for (i, e) in events.iter().enumerate() {
        if e.seq.0 > until.0 {
            break;
        }
        if matches!(e.kind, EventKind::UserMsg) {
            last_user_idx = Some(i);
        }
    }
    let start = match last_user_idx {
        Some(i) => i + 1,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for e in &events[start..] {
        if e.seq.0 > until.0 {
            break;
        }
        match e.kind {
            EventKind::ToolUse => {
                if let Ok(p) = e.decode_payload::<ToolUsePayload>() {
                    out.push(TrajectoryEntry::ToolUse {
                        ordinal: p.ordinal,
                        name: p.tool_name,
                        args: p.args_json,
                    });
                }
            }
            EventKind::ToolResult => {
                if let Ok(p) = e.decode_payload::<ToolResultPayload>() {
                    out.push(TrajectoryEntry::ToolResult {
                        ordinal: p.ordinal,
                        outcome: p.outcome,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn extract_user_intent(events: &[EventRecord], until: EventSeq) -> Option<String> {
    let mut last_user: Option<&EventRecord> = None;
    for e in events {
        if e.seq.0 > until.0 {
            break;
        }
        if matches!(e.kind, EventKind::UserMsg) {
            last_user = Some(e);
        }
    }
    last_user.and_then(|e| {
        e.decode_payload::<JsonValue>()
            .ok()
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
            .or_else(|| {
                // Fallback: try a top-level string payload.
                e.decode_payload::<String>().ok()
            })
    })
}

fn pair_and_sanitize(
    trajectory: &[TrajectoryEntry],
    report: &mut SanitizationReport,
) -> (Vec<SanitizedStep>, bool) {
    // Build an ordinal -> Result map from the results.
    let mut results: BTreeMap<u32, Result<JsonValue, String>> = BTreeMap::new();
    for t in trajectory {
        if let TrajectoryEntry::ToolResult { ordinal, outcome } = t {
            results.insert(*ordinal, outcome.clone());
        }
    }
    let mut had_failure = false;
    let mut steps = Vec::new();
    for t in trajectory {
        if let TrajectoryEntry::ToolUse { ordinal, name, args } = t {
            // A tool_use without a matching tool_result is treated
            // as a failure (the conversation FSM normally synthesizes
            // a cancellation result; we still classify it as failure
            // for capture purposes).
            let result = results
                .get(ordinal)
                .cloned()
                .unwrap_or(Err("missing tool_result".into()));
            if result.is_err() {
                had_failure = true;
            }
            steps.push(sanitize_step(*ordinal, name, args, &result, report));
        }
    }
    (steps, had_failure)
}

fn log_outcome(req: &CaptureRequest, outcome: &CaptureOutcome) {
    use CaptureOutcome::*;
    match outcome {
        Disabled => {
            tracing::trace!(
                conversation_id = %req.conversation_id.as_str(),
                "auto-capture disabled; skipping turn"
            );
        }
        BelowThreshold { tool_calls, threshold } => {
            tracing::debug!(
                conversation_id = %req.conversation_id.as_str(),
                tool_calls,
                threshold,
                "auto-capture: below threshold"
            );
        }
        HadFailure => {
            tracing::debug!(
                conversation_id = %req.conversation_id.as_str(),
                "auto-capture: trajectory had a failed tool call"
            );
        }
        Skipped { reason } => {
            tracing::info!(
                conversation_id = %req.conversation_id.as_str(),
                reason = %reason,
                "auto-capture: summarizer returned SKIP"
            );
        }
        DryRun { proposal } => {
            tracing::info!(
                conversation_id = %req.conversation_id.as_str(),
                proposed_name = %proposal.name,
                "auto-capture: dry-run produced a proposal"
            );
        }
        Created { name } => {
            tracing::info!(
                conversation_id = %req.conversation_id.as_str(),
                skill = %name,
                "auto-capture: created draft skill"
            );
        }
        Blocked { name, reason } => {
            tracing::warn!(
                conversation_id = %req.conversation_id.as_str(),
                skill = %name,
                reason = %reason,
                "auto-capture: scanner blocked the write — sanitizer let something through"
            );
        }
        Conflict { name } => {
            tracing::info!(
                conversation_id = %req.conversation_id.as_str(),
                skill = %name,
                "auto-capture: name already in use; skipping"
            );
        }
        Error { message } => {
            tracing::warn!(
                conversation_id = %req.conversation_id.as_str(),
                error = %message,
                "auto-capture pipeline error"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarizer::{DraftSkillProposal, SummarizerOutput};
    use async_trait::async_trait;
    use execlaw_core::db::DbConfig;
    use execlaw_core::events::{EventLog, EventRecord};
    use execlaw_core::ids::{ConversationId, EventSeq};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::skills_config::{SkillsConfigStore, SkillsConfigUpdate};
    use execlaw_core::Database;
    use serde_json::json;
    use std::sync::Mutex;

    fn fresh() -> (Database, Arc<SkillStore>) {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = Arc::new(SkillStore::new(db.clone()));
        (db, store)
    }

    /// Mock summarizer that returns a fixed reply.
    struct MockSummarizer {
        reply: SummarizerOutput,
        calls: Mutex<usize>,
    }
    impl MockSummarizer {
        fn new(reply: SummarizerOutput) -> Self {
            Self {
                reply,
                calls: Mutex::new(0),
            }
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }
    #[async_trait]
    impl SkillSummarizer for MockSummarizer {
        async fn summarize(&self, _p: SummarizerPrompt) -> Result<SummarizerOutput, String> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.reply.clone())
        }
    }

    fn enable_capture(db: &Database, min: u32) {
        SkillsConfigStore::new(db)
            .update(
                &SkillsConfigUpdate {
                    auto_capture_enabled: Some(true),
                    auto_capture_min_tool_calls: Some(min),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
    }

    fn append_user(log: &EventLog, cid: &ConversationId, seq: i64, text: &str) {
        log.append(
            &EventRecord::new(
                cid.clone(),
                EventSeq(seq),
                EventKind::UserMsg,
                &json!({"text": text}),
                Some("user".into()),
            )
            .unwrap(),
        )
        .unwrap();
    }

    fn append_tool_use(log: &EventLog, cid: &ConversationId, seq: i64, ordinal: u32, name: &str, args: serde_json::Value) {
        log.append(
            &EventRecord::new(
                cid.clone(),
                EventSeq(seq),
                EventKind::ToolUse,
                &ToolUsePayload {
                    ordinal,
                    tool_name: name.into(),
                    args_json: args,
                },
                None,
            )
            .unwrap(),
        )
        .unwrap();
    }

    fn append_tool_result(log: &EventLog, cid: &ConversationId, seq: i64, ordinal: u32, outcome: Result<serde_json::Value, String>) {
        log.append(
            &EventRecord::new(
                cid.clone(),
                EventSeq(seq),
                EventKind::ToolResult,
                &ToolResultPayload { ordinal, outcome },
                None,
            )
            .unwrap(),
        )
        .unwrap();
    }

    async fn run(
        db: &Database,
        store: &Arc<SkillStore>,
        summarizer: Arc<MockSummarizer>,
        cid: &ConversationId,
        until: i64,
    ) -> CaptureOutcome {
        let worker = AutoCaptureWorker::new(db.clone(), store.clone(), summarizer);
        worker
            .process_request(CaptureRequest {
                conversation_id: cid.clone(),
                until_seq: EventSeq(until),
                run_id: "test-run".into(),
            })
            .await
    }

    // --- short-circuits ---

    #[tokio::test]
    async fn disabled_when_config_off() {
        let (db, store) = fresh();
        let cid = ConversationId::from("c1");
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "n/a".into(),
        }));
        let outcome = run(&db, &store, summ.clone(), &cid, 0).await;
        assert_eq!(outcome, CaptureOutcome::Disabled);
        assert_eq!(summ.calls(), 0, "summarizer must not be called");
    }

    #[tokio::test]
    async fn below_threshold_short_circuits_before_summarizer() {
        let (db, store) = fresh();
        enable_capture(&db, 5);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do a thing");
        append_tool_use(&log, &cid, 2, 0, "search", json!({"q": "x"}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!([])));

        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "n/a".into(),
        }));
        let outcome = run(&db, &store, summ.clone(), &cid, 3).await;
        assert!(matches!(
            outcome,
            CaptureOutcome::BelowThreshold { tool_calls: 1, threshold: 5 }
        ));
        assert_eq!(summ.calls(), 0);
    }

    #[tokio::test]
    async fn failed_tool_call_in_trajectory_aborts_capture() {
        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Err("boom".into()));
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "n/a".into(),
        }));
        let outcome = run(&db, &store, summ.clone(), &cid, 3).await;
        assert_eq!(outcome, CaptureOutcome::HadFailure);
        assert_eq!(summ.calls(), 0);
    }

    // --- happy path ---

    #[tokio::test]
    async fn successful_trajectory_creates_draft_skill() {
        let (db, store) = fresh();
        enable_capture(&db, 2);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "scaffold a crate");
        for i in 0..3 {
            let s = 2 + i * 2;
            append_tool_use(&log, &cid, s, i as u32, "shell", json!({"cmd": "cargo new"}));
            append_tool_result(&log, &cid, s + 1, i as u32, Ok(json!({"ok": true})));
        }
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "dev/scaffold".into(),
                description: "scaffold a crate".into(),
                body_md: "1. cargo new\n2. cd\n3. cargo check".into(),
                tags: vec!["dev".into()],
            },
        )));
        let outcome = run(&db, &store, summ.clone(), &cid, 100).await;
        assert!(
            matches!(outcome, CaptureOutcome::Created { ref name } if name == "dev/scaffold"),
            "{outcome:?}"
        );
        assert_eq!(summ.calls(), 1);

        // Skill landed.
        let g = store.get("dev/scaffold").unwrap().unwrap();
        assert_eq!(g.source, "agent:test-run");
        assert_eq!(g.current_version.body_md, "1. cargo new\n2. cd\n3. cargo check");
    }

    #[tokio::test]
    async fn dry_run_runs_summarizer_but_does_not_write() {
        let (db, store) = fresh();
        SkillsConfigStore::new(&db)
            .update(
                &SkillsConfigUpdate {
                    auto_capture_enabled: Some(false),
                    auto_capture_dry_run: Some(true),
                    auto_capture_min_tool_calls: Some(1),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "x/y".into(),
                description: "d".into(),
                body_md: "b".into(),
                tags: vec![],
            },
        )));
        let outcome = run(&db, &store, summ.clone(), &cid, 100).await;
        assert!(matches!(outcome, CaptureOutcome::DryRun { .. }));
        assert_eq!(summ.calls(), 1);
        assert!(store.get("x/y").unwrap().is_none(), "no row written");
    }

    // --- adversarial ---

    #[tokio::test]
    async fn summarizer_returning_invalid_name_is_logged_as_error_not_written() {
        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "Bad Name With Spaces".into(),
                description: "d".into(),
                body_md: "b".into(),
                tags: vec![],
            },
        )));
        let outcome = run(&db, &store, summ, &cid, 100).await;
        assert!(matches!(outcome, CaptureOutcome::Error { .. }));
        // Database shouldn't have any skill rows.
        let count: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM state_skills", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn summarizer_returning_secret_in_body_is_blocked_by_scanner() {
        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "leak/sneaky".into(),
                description: "d".into(),
                body_md: "use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to call".into(),
                tags: vec![],
            },
        )));
        let outcome = run(&db, &store, summ, &cid, 100).await;
        assert!(matches!(outcome, CaptureOutcome::Blocked { .. }), "{outcome:?}");
        assert!(store.get("leak/sneaky").unwrap().is_none());
    }

    #[tokio::test]
    async fn name_conflict_with_existing_skill_returns_conflict_outcome() {
        let (db, store) = fresh();
        enable_capture(&db, 1);
        // Pre-author a skill that the summarizer will try to clobber.
        store
            .create(
                NewSkill {
                    name: "x/dup".into(),
                    source: "admin".into(),
                    registration_kind: RegistrationKind::Authored,
                    owning_plugin_id: None,
                    initial_version: NewSkillVersion {
                        description: "existing".into(),
                        body_md: "by admin".into(),
                        frontmatter_json: "{}".into(),
                        authored_by: "admin".into(),
                        promotion_notes: None,
                    },
                    resources: vec![],
                },
                Strictness::Strict,
                100,
            )
            .unwrap();
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "x/dup".into(),
                description: "agent's draft".into(),
                body_md: "agent body".into(),
                tags: vec![],
            },
        )));
        let outcome = run(&db, &store, summ, &cid, 100).await;
        assert!(matches!(outcome, CaptureOutcome::Conflict { .. }), "{outcome:?}");
        // Admin's row is unchanged.
        let g = store.get("x/dup").unwrap().unwrap();
        assert_eq!(g.current_version.body_md, "by admin");
    }

    #[tokio::test]
    async fn summarizer_skip_does_not_write_anything() {
        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "too narrow".into(),
        }));
        let outcome = run(&db, &store, summ, &cid, 100).await;
        assert!(matches!(outcome, CaptureOutcome::Skipped { .. }));
    }

    // --- user intent sanitization ---

    #[tokio::test]
    async fn user_intent_with_credentials_is_sanitized_before_summarizer() {
        // Audit fix coverage (2026-05-03): operator messages
        // containing credentials must not reach the summarizer
        // verbatim. Capture the prompt the summarizer receives and
        // assert the credential is gone.
        struct CapturingSummarizer {
            seen_intent: std::sync::Mutex<Option<String>>,
        }
        #[async_trait]
        impl SkillSummarizer for CapturingSummarizer {
            async fn summarize(&self, p: SummarizerPrompt) -> Result<SummarizerOutput, String> {
                *self.seen_intent.lock().unwrap() = p.user_intent.clone();
                Ok(SummarizerOutput::Skip {
                    reason: "test capture".into(),
                })
            }
        }

        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        // Operator pastes a token into chat (rare but happens).
        append_user(
            &log,
            &cid,
            1,
            "use my key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to fetch",
        );
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));

        let summ = Arc::new(CapturingSummarizer {
            seen_intent: std::sync::Mutex::new(None),
        });
        let worker = AutoCaptureWorker::new(db.clone(), store.clone(), summ.clone());
        let _ = worker
            .process_request(CaptureRequest {
                conversation_id: cid.clone(),
                until_seq: EventSeq(3),
                run_id: "r".into(),
            })
            .await;
        let intent = summ.seen_intent.lock().unwrap().clone();
        let intent = intent.expect("summarizer must have been called with a user_intent");
        assert!(
            !intent.contains("sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz"),
            "credential leaked into summarizer prompt: {intent:?}"
        );
        assert!(
            intent.contains("<<redacted-secret>>") || intent.contains("redacted"),
            "expected redaction sentinel in {intent:?}"
        );
    }

    // --- trajectory extraction ---

    #[tokio::test]
    async fn trajectory_starts_at_last_user_msg_not_first() {
        let (db, store) = fresh();
        enable_capture(&db, 2);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        // Earlier, unrelated turn — must be IGNORED.
        append_user(&log, &cid, 1, "older request");
        append_tool_use(&log, &cid, 2, 0, "old", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        // Current turn.
        append_user(&log, &cid, 4, "current request");
        append_tool_use(&log, &cid, 5, 1, "new1", json!({}));
        append_tool_result(&log, &cid, 6, 1, Ok(json!({})));
        append_tool_use(&log, &cid, 7, 2, "new2", json!({}));
        append_tool_result(&log, &cid, 8, 2, Ok(json!({})));

        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "new/proc".into(),
                description: "d".into(),
                body_md: "b".into(),
                tags: vec![],
            },
        )));
        let worker = AutoCaptureWorker::new(db.clone(), store.clone(), summ);
        let outcome = worker
            .process_request(CaptureRequest {
                conversation_id: cid.clone(),
                until_seq: EventSeq(8),
                run_id: "r".into(),
            })
            .await;
        // Two tool calls in current turn (>= threshold of 2).
        assert!(matches!(outcome, CaptureOutcome::Created { .. }));
    }

    // --- sink ---

    #[test]
    fn noop_sink_returns_false_on_enqueue_and_does_not_panic() {
        let sink = AutoCaptureSink::noop();
        let req = CaptureRequest {
            conversation_id: ConversationId::from("c1"),
            until_seq: EventSeq(0),
            run_id: "x".into(),
        };
        assert!(!sink.enqueue(req));
    }

    /// Audit regression (2026-05-03): a panic in `process_request`
    /// must NOT kill the worker loop. Subsequent enqueues must
    /// still be processed. Mock summarizer that panics on first
    /// call + succeeds on second.
    #[tokio::test]
    async fn worker_survives_panic_in_one_request() {
        // AtomicUsize so the panic in call 1 doesn't poison the
        // counter; call 2 must execute cleanly.
        struct PanicOnceSummarizer {
            calls: std::sync::atomic::AtomicUsize,
            success_reply: SummarizerOutput,
        }
        #[async_trait]
        impl SkillSummarizer for PanicOnceSummarizer {
            async fn summarize(&self, _p: SummarizerPrompt) -> Result<SummarizerOutput, String> {
                let n = self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                if n == 1 {
                    panic!("simulated summarizer crash");
                }
                Ok(self.success_reply.clone())
            }
        }

        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        // Two complete turns in the same conversation.
        append_user(&log, &cid, 1, "first");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));
        append_user(&log, &cid, 4, "second");
        append_tool_use(&log, &cid, 5, 1, "t", json!({}));
        append_tool_result(&log, &cid, 6, 1, Ok(json!({})));

        let summ = Arc::new(PanicOnceSummarizer {
            calls: std::sync::atomic::AtomicUsize::new(0),
            success_reply: SummarizerOutput::Draft(DraftSkillProposal {
                name: "post/panic".into(),
                description: "after panic".into(),
                body_md: "b".into(),
                tags: vec![],
            }),
        });
        let worker = Arc::new(AutoCaptureWorker::new(db.clone(), store.clone(), summ));
        let (sink, _handle) = worker.spawn();

        // Enqueue both turns.
        sink.enqueue(CaptureRequest {
            conversation_id: cid.clone(),
            until_seq: EventSeq(3),
            run_id: "r1".into(),
        });
        sink.enqueue(CaptureRequest {
            conversation_id: cid.clone(),
            until_seq: EventSeq(6),
            run_id: "r2".into(),
        });
        // Drop the sink so the receiver eventually sees no more
        // senders and the worker loop can exit cleanly.
        drop(sink);
        // Wait for the worker to finish processing.
        // The handle is the OUTER loop; it exits once rx.recv()
        // returns None. Use a small bounded wait since the panic
        // scenario could otherwise hang the test.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), _handle).await;

        // The post-panic skill MUST have been written, proving the
        // worker survived the panic and processed the second turn.
        assert!(
            store.get("post/panic").unwrap().is_some(),
            "worker did not survive panic in earlier request"
        );
    }

    #[tokio::test]
    async fn worker_spawn_drives_pipeline_via_sink() {
        let (db, store) = fresh();
        enable_capture(&db, 1);
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append_user(&log, &cid, 1, "do");
        append_tool_use(&log, &cid, 2, 0, "t", json!({}));
        append_tool_result(&log, &cid, 3, 0, Ok(json!({})));

        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "spawn/test".into(),
                description: "d".into(),
                body_md: "b".into(),
                tags: vec![],
            },
        )));
        let worker = Arc::new(AutoCaptureWorker::new(db.clone(), store.clone(), summ));
        let (sink, handle) = worker.spawn();
        sink.enqueue(CaptureRequest {
            conversation_id: cid.clone(),
            until_seq: EventSeq(100),
            run_id: "r".into(),
        });
        // Drop the sink so the receiver closes after draining.
        drop(sink);
        // Wait for the spawned task to finish processing.
        handle.await.unwrap();
        assert!(store.get("spawn/test").unwrap().is_some());
    }
}
