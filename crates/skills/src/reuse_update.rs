//! Phase D.3 — reuse-update worker.
//!
//! Sibling of [`crate::capture::AutoCaptureWorker`]. Runs after a
//! `skills.view` activation closes (chat handler at turn end), and
//! evaluates whether the closed turn revealed an improvement worth
//! saving as a NEW VERSION (fork) of the existing skill.
//!
//! Architecture:
//!
//! ```text
//!  chat handler (turn end)
//!      └─ store.close_open_invocations(...) ──► (inv_id, skill_id) pairs
//!         for each pair: sink.enqueue(ReuseUpdateRequest{...})
//!                                                        ▼
//!                                              mpsc::UnboundedReceiver
//!                                                        │
//!                                              ReuseUpdateWorker.spawn()
//!                                                        │
//!                                          replay → sanitize → improve-eval
//!                                                        │
//!                                                  Skip │ Draft (fork)
//!                                                        ▼
//!                                            SkillStore::submit_proposal
//!                                                  (kind=VersionFork)
//! ```
//!
//! Always opt-in: `SkillsConfig::reuse_update_enabled = false`
//! short-circuits at the top, so an idle operator pays only the cost
//! of one config read per closed invocation.
//!
//! Always proposes — never mutates `stable` skills directly. The
//! operator reviews the fork on the Skills > Proposals tab; on
//! approve, `SkillStore::approve_proposal` runs `add_version`
//! against the target skill.

use crate::capture::CaptureOutcome;
use crate::model::{NewProposal, ProposalKind, SkillError, SkillId};
use crate::sanitizer::{SanitizationReport, sanitize_step};
use crate::store::SkillStore;
use crate::summarizer::{
    SkillSummarizer, SummarizerOutput, SummarizerPrompt, build_improvement_prompt,
};
use execlaw_core::events::{EventKind, EventLog, EventRecord, ToolResultPayload, ToolUsePayload};
use execlaw_core::ids::{ConversationId, EventSeq};
use execlaw_core::skills_config::SkillsConfigStore;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Pushed by the chat handler for each invocation closed at turn end.
#[derive(Debug, Clone)]
pub struct ReuseUpdateRequest {
    pub conversation_id: ConversationId,
    pub invocation_id: i64,
    pub skill_id: SkillId,
    pub until_seq: EventSeq,
    pub run_id: String,
    /// `success`, `failure`, or `aborted`. Only `success` is
    /// summarized today (failures may add value later but the
    /// initial heuristic is that improvements come from things
    /// that worked).
    pub outcome: String,
}

#[derive(Clone)]
pub struct ReuseUpdateSink {
    tx: Option<mpsc::UnboundedSender<ReuseUpdateRequest>>,
}

impl ReuseUpdateSink {
    pub fn noop() -> Self {
        Self { tx: None }
    }
    pub fn enqueue(&self, req: ReuseUpdateRequest) -> bool {
        match &self.tx {
            None => false,
            Some(tx) => tx.send(req).is_ok(),
        }
    }
}

pub struct ReuseUpdateWorker {
    db: execlaw_core::Database,
    skill_store: Arc<SkillStore>,
    summarizer: Arc<dyn SkillSummarizer>,
    pub summarizer_max_tokens: u32,
}

impl ReuseUpdateWorker {
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

    pub fn spawn(self: Arc<Self>) -> (ReuseUpdateSink, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<ReuseUpdateRequest>();
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
                            invocation_id = req.invocation_id,
                            "reuse-update pipeline PANICKED; worker continues"
                        );
                    }
                }
            }
        });
        (ReuseUpdateSink { tx: Some(tx) }, handle)
    }

    pub async fn process_request(&self, req: ReuseUpdateRequest) -> CaptureOutcome {
        // 1. Config gate.
        let cfg = match SkillsConfigStore::new(&self.db).get() {
            Ok(c) => c,
            Err(e) => {
                return CaptureOutcome::Error {
                    message: e.to_string(),
                };
            }
        };
        if !cfg.reuse_update_enabled {
            return CaptureOutcome::Disabled;
        }

        // 2. Only learn from successful invocations for now.
        if req.outcome != "success" {
            return CaptureOutcome::HadFailure;
        }

        // 3. Look up the target skill (must exist + not be archived).
        let skill_name = match self.lookup_skill_name(req.skill_id) {
            Ok(Some(n)) => n,
            Ok(None) => {
                return CaptureOutcome::Error {
                    message: format!("skill {} not found", req.skill_id.0),
                };
            }
            Err(e) => {
                return CaptureOutcome::Error {
                    message: e.to_string(),
                };
            }
        };
        let view = match self.skill_store.view(&skill_name) {
            Ok(Some(v)) => v,
            Ok(None) => return CaptureOutcome::Disabled, // archived; nothing to fork
            Err(e) => {
                return CaptureOutcome::Error {
                    message: e.to_string(),
                };
            }
        };

        // 4. Replay and slice trajectory (same logic as auto-capture).
        let log = EventLog::new(&self.db);
        let events = match log.replay_since(&req.conversation_id, EventSeq(0)) {
            Ok(v) => v,
            Err(e) => {
                return CaptureOutcome::Error {
                    message: e.to_string(),
                };
            }
        };
        let trajectory = extract_latest_trajectory(&events, req.until_seq);

        // Below threshold cap: improvement evaluation needs at least
        // one tool call to evaluate; without that the trajectory is
        // just the activation itself.
        let tool_call_count = trajectory.iter().filter(|t| t.is_tool_use()).count();
        if tool_call_count == 0 {
            return CaptureOutcome::BelowThreshold {
                tool_calls: 0,
                threshold: 1,
            };
        }

        // 5. Pair + sanitize.
        let mut report = SanitizationReport::default();
        let (steps, _had_failure) = pair_and_sanitize(&trajectory, &mut report);
        if steps.is_empty() {
            return CaptureOutcome::BelowThreshold {
                tool_calls: 0,
                threshold: 1,
            };
        }

        // 6. Build improvement prompt + summarize.
        let user_intent = extract_user_intent(&events, req.until_seq);
        let prompt = SummarizerPrompt {
            steps,
            user_intent,
            max_tokens: self.summarizer_max_tokens,
        };
        let (system, user) = build_improvement_prompt(&skill_name, &view.body_md, &prompt);
        let raw = match self
            .summarizer
            .summarize(SummarizerPrompt {
                // Reuse the trait — pass the COMBINED prompt by
                // hijacking the user_intent field so the impl does
                // a single chat call. This works because the
                // production InferenceSummarizer joins system+user
                // from build_prompt; we pre-build them here and
                // bypass that. To keep the trait stable, we pass
                // a synthetic prompt whose steps are empty and
                // user_intent carries the full user message.
                steps: vec![],
                user_intent: Some(format!("{system}\n\n---\n\n{user}")),
                max_tokens: self.summarizer_max_tokens,
            })
            .await
        {
            Ok(SummarizerOutput::Skip { reason }) => {
                return CaptureOutcome::Skipped { reason };
            }
            Ok(SummarizerOutput::Draft(d)) => d,
            Err(e) => return CaptureOutcome::Error { message: e },
        };

        // 7. Validate the proposal name matches the existing skill.
        if raw.name != skill_name {
            return CaptureOutcome::Error {
                message: format!(
                    "improvement evaluator returned wrong skill name: got {:?}, expected {:?}",
                    raw.name, skill_name
                ),
            };
        }

        // 8. Submit as VersionFork proposal.
        let frontmatter = serde_json::json!({
            "name": raw.name,
            "description": raw.description,
            "tags": raw.tags,
            "agent_proposed": true,
            "kind": "version_fork",
            "run_id": req.run_id,
            "invocation_id": req.invocation_id,
        })
        .to_string();
        let now_ms = chrono::Utc::now().timestamp() * 1000;
        let new_p = NewProposal {
            kind: ProposalKind::VersionFork,
            target_skill_id: Some(req.skill_id),
            proposed_name: raw.name.clone(),
            description: raw.description.clone(),
            body_md: raw.body_md.clone(),
            frontmatter_json: frontmatter,
            source_run_id: req.run_id.clone(),
            trajectory_summary: Some(format!(
                "reuse-update from invocation {} ({} tool calls)",
                req.invocation_id, tool_call_count
            )),
            tool_calls_observed: tool_call_count as u32,
        };
        match self.skill_store.submit_proposal(new_p, now_ms) {
            Ok(_) => CaptureOutcome::DryRun { proposal: raw },
            Err(SkillError::Blocked { findings, fields }) => CaptureOutcome::Blocked {
                name: raw.name,
                reason: format!("scanner blocked: {findings} finding(s) in {fields:?}"),
            },
            Err(e) => CaptureOutcome::Error {
                message: e.to_string(),
            },
        }
    }

    fn lookup_skill_name(&self, id: SkillId) -> Result<Option<String>, execlaw_core::DbError> {
        // Returns the name regardless of state. The caller's `view()`
        // step then uses the regular skill-store visibility rules
        // (archived → not visible → return Disabled).
        self.db.with_conn(|c| {
            let n = c
                .query_row(
                    "SELECT name FROM state_skills WHERE id = ?1",
                    rusqlite::params![id.0],
                    |r| r.get::<_, String>(0),
                )
                .ok();
            Ok(n)
        })
    }
}

#[derive(Debug, Clone)]
enum TrajectoryEntry {
    ToolUse {
        ordinal: u32,
        name: String,
        args: JsonValue,
    },
    ToolResult {
        ordinal: u32,
        outcome: Result<JsonValue, String>,
    },
}

impl TrajectoryEntry {
    fn is_tool_use(&self) -> bool {
        matches!(self, TrajectoryEntry::ToolUse { .. })
    }
}

fn extract_latest_trajectory(events: &[EventRecord], until: EventSeq) -> Vec<TrajectoryEntry> {
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
            .or_else(|| e.decode_payload::<String>().ok())
    })
}

fn pair_and_sanitize(
    trajectory: &[TrajectoryEntry],
    report: &mut SanitizationReport,
) -> (Vec<crate::sanitizer::SanitizedStep>, bool) {
    let mut results: BTreeMap<u32, Result<JsonValue, String>> = BTreeMap::new();
    for t in trajectory {
        if let TrajectoryEntry::ToolResult { ordinal, outcome } = t {
            results.insert(*ordinal, outcome.clone());
        }
    }
    let mut had_failure = false;
    let mut steps = Vec::new();
    for t in trajectory {
        if let TrajectoryEntry::ToolUse {
            ordinal,
            name,
            args,
        } = t
        {
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

fn log_outcome(req: &ReuseUpdateRequest, outcome: &CaptureOutcome) {
    use CaptureOutcome::*;
    match outcome {
        Disabled => tracing::trace!(
            invocation_id = req.invocation_id,
            "reuse-update disabled or skill archived; skipping"
        ),
        BelowThreshold {
            tool_calls,
            threshold,
        } => tracing::debug!(
            invocation_id = req.invocation_id,
            tool_calls,
            threshold,
            "reuse-update: trajectory too small to evaluate"
        ),
        HadFailure => tracing::debug!(
            invocation_id = req.invocation_id,
            outcome = %req.outcome,
            "reuse-update: outcome was non-success; skipping"
        ),
        Skipped { reason } => tracing::info!(
            invocation_id = req.invocation_id,
            reason = %reason,
            "reuse-update: improvement evaluator returned SKIP"
        ),
        DryRun { proposal } => tracing::info!(
            invocation_id = req.invocation_id,
            skill = %proposal.name,
            "reuse-update: produced version-fork proposal"
        ),
        Created { name } => tracing::info!(
            invocation_id = req.invocation_id,
            skill = %name,
            "reuse-update: unexpected Created outcome (worker only proposes)"
        ),
        Blocked { name, reason } => tracing::warn!(
            invocation_id = req.invocation_id,
            skill = %name,
            reason = %reason,
            "reuse-update: scanner blocked the proposal — sanitizer let something through"
        ),
        Conflict { name } => tracing::info!(
            invocation_id = req.invocation_id,
            skill = %name,
            "reuse-update: conflict (unexpected for fork proposals)"
        ),
        Error { message } => tracing::warn!(
            invocation_id = req.invocation_id,
            error = %message,
            "reuse-update pipeline error"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewSkill, NewSkillVersion, RegistrationKind};
    use crate::summarizer::{DraftSkillProposal, SummarizerOutput};
    use async_trait::async_trait;
    use execlaw_core::Database;
    use execlaw_core::db::DbConfig;
    use execlaw_core::events::{EventLog, EventRecord};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::skills_config::{SkillsConfigStore, SkillsConfigUpdate};
    use serde_json::json;
    use std::sync::Mutex;

    fn fresh() -> (Database, Arc<SkillStore>) {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        (db.clone(), Arc::new(SkillStore::new(db)))
    }

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

    fn enable_reuse_update(db: &Database) {
        SkillsConfigStore::new(db)
            .update(
                &SkillsConfigUpdate {
                    reuse_update_enabled: Some(true),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
    }

    fn append(
        log: &EventLog,
        cid: &ConversationId,
        seq: i64,
        kind: EventKind,
        payload: &impl serde::Serialize,
    ) {
        log.append(&EventRecord::new(cid.clone(), EventSeq(seq), kind, payload, None).unwrap())
            .unwrap();
    }

    fn seed_skill(store: &Arc<SkillStore>, name: &str, body: &str) -> SkillId {
        store
            .create(
                NewSkill {
                    name: name.into(),
                    source: "admin".into(),
                    registration_kind: RegistrationKind::Authored,
                    owning_plugin_id: None,
                    initial_version: NewSkillVersion {
                        description: "test".into(),
                        body_md: body.into(),
                        frontmatter_json: "{}".into(),
                        authored_by: "admin".into(),
                        promotion_notes: None,
                    },
                    resources: vec![],
                },
                crate::scanner::Strictness::Strict,
                10,
            )
            .unwrap()
    }

    #[tokio::test]
    async fn disabled_when_config_off() {
        let (db, store) = fresh();
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "n/a".into(),
        }));
        let w = ReuseUpdateWorker::new(db, store.clone(), summ.clone());
        let outcome = w
            .process_request(ReuseUpdateRequest {
                conversation_id: ConversationId::from("c1"),
                invocation_id: 1,
                skill_id: SkillId(1),
                until_seq: EventSeq(0),
                run_id: "r".into(),
                outcome: "success".into(),
            })
            .await;
        assert_eq!(outcome, CaptureOutcome::Disabled);
        assert_eq!(summ.calls(), 0);
    }

    #[tokio::test]
    async fn skips_non_success_outcomes() {
        let (db, store) = fresh();
        enable_reuse_update(&db);
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "n/a".into(),
        }));
        let w = ReuseUpdateWorker::new(db, store.clone(), summ.clone());
        let outcome = w
            .process_request(ReuseUpdateRequest {
                conversation_id: ConversationId::from("c1"),
                invocation_id: 1,
                skill_id: SkillId(1),
                until_seq: EventSeq(0),
                run_id: "r".into(),
                outcome: "failure".into(),
            })
            .await;
        assert_eq!(outcome, CaptureOutcome::HadFailure);
        assert_eq!(summ.calls(), 0);
    }

    #[tokio::test]
    async fn produces_version_fork_proposal_on_improvement() {
        let (db, store) = fresh();
        enable_reuse_update(&db);
        let id = seed_skill(&store, "research/sources", "v1: vague");
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append(
            &log,
            &cid,
            1,
            EventKind::UserMsg,
            &json!({"text": "find me sources"}),
        );
        append(
            &log,
            &cid,
            2,
            EventKind::ToolUse,
            &ToolUsePayload {
                ordinal: 0,
                tool_name: "search".into(),
                args_json: json!({"q": "rust"}),
            },
        );
        append(
            &log,
            &cid,
            3,
            EventKind::ToolResult,
            &ToolResultPayload {
                ordinal: 0,
                outcome: Ok(json!([])),
            },
        );

        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "research/sources".into(),
                description: "improved".into(),
                body_md: "v2: clearer".into(),
                tags: vec![],
            },
        )));
        let w = ReuseUpdateWorker::new(db, store.clone(), summ);
        let outcome = w
            .process_request(ReuseUpdateRequest {
                conversation_id: cid,
                invocation_id: 99,
                skill_id: id,
                until_seq: EventSeq(3),
                run_id: "r".into(),
                outcome: "success".into(),
            })
            .await;
        assert!(
            matches!(outcome, CaptureOutcome::DryRun { .. }),
            "{outcome:?}"
        );

        // Proposal landed.
        let pending = store
            .list_proposals(Some(crate::model::ProposalState::Pending))
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ProposalKind::VersionFork);
        assert_eq!(pending[0].target_skill_id, Some(id));
        assert_eq!(pending[0].body_md, "v2: clearer");
    }

    #[tokio::test]
    async fn skipped_when_summarizer_returns_skip() {
        let (db, store) = fresh();
        enable_reuse_update(&db);
        let id = seed_skill(&store, "research/sources", "v1");
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append(&log, &cid, 1, EventKind::UserMsg, &json!({"text": "find"}));
        append(
            &log,
            &cid,
            2,
            EventKind::ToolUse,
            &ToolUsePayload {
                ordinal: 0,
                tool_name: "t".into(),
                args_json: json!({}),
            },
        );
        append(
            &log,
            &cid,
            3,
            EventKind::ToolResult,
            &ToolResultPayload {
                ordinal: 0,
                outcome: Ok(json!({})),
            },
        );
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "no improvement found".into(),
        }));
        let w = ReuseUpdateWorker::new(db, store.clone(), summ);
        let outcome = w
            .process_request(ReuseUpdateRequest {
                conversation_id: cid,
                invocation_id: 1,
                skill_id: id,
                until_seq: EventSeq(3),
                run_id: "r".into(),
                outcome: "success".into(),
            })
            .await;
        assert!(matches!(outcome, CaptureOutcome::Skipped { .. }));
        assert!(store.list_proposals(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_summarizer_returning_wrong_skill_name() {
        let (db, store) = fresh();
        enable_reuse_update(&db);
        let id = seed_skill(&store, "research/sources", "v1");
        let cid = ConversationId::from("c1");
        let log = EventLog::new(&db);
        append(&log, &cid, 1, EventKind::UserMsg, &json!({"text": "x"}));
        append(
            &log,
            &cid,
            2,
            EventKind::ToolUse,
            &ToolUsePayload {
                ordinal: 0,
                tool_name: "t".into(),
                args_json: json!({}),
            },
        );
        append(
            &log,
            &cid,
            3,
            EventKind::ToolResult,
            &ToolResultPayload {
                ordinal: 0,
                outcome: Ok(json!({})),
            },
        );
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Draft(
            DraftSkillProposal {
                name: "wrong/name".into(),
                description: "d".into(),
                body_md: "b".into(),
                tags: vec![],
            },
        )));
        let w = ReuseUpdateWorker::new(db, store.clone(), summ);
        let outcome = w
            .process_request(ReuseUpdateRequest {
                conversation_id: cid,
                invocation_id: 1,
                skill_id: id,
                until_seq: EventSeq(3),
                run_id: "r".into(),
                outcome: "success".into(),
            })
            .await;
        assert!(matches!(outcome, CaptureOutcome::Error { .. }));
        assert!(store.list_proposals(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn fork_for_archived_skill_short_circuits_disabled() {
        let (db, store) = fresh();
        enable_reuse_update(&db);
        let id = seed_skill(&store, "research/sources", "v1");
        store.archive("research/sources", 100).unwrap();
        let summ = Arc::new(MockSummarizer::new(SummarizerOutput::Skip {
            reason: "n/a".into(),
        }));
        let w = ReuseUpdateWorker::new(db, store.clone(), summ.clone());
        let outcome = w
            .process_request(ReuseUpdateRequest {
                conversation_id: ConversationId::from("c1"),
                invocation_id: 1,
                skill_id: id,
                until_seq: EventSeq(0),
                run_id: "r".into(),
                outcome: "success".into(),
            })
            .await;
        assert_eq!(outcome, CaptureOutcome::Disabled);
        assert_eq!(
            summ.calls(),
            0,
            "summarizer must not be called for archived target"
        );
    }

    #[test]
    fn noop_sink_returns_false() {
        let s = ReuseUpdateSink::noop();
        assert!(!s.enqueue(ReuseUpdateRequest {
            conversation_id: ConversationId::from("c"),
            invocation_id: 1,
            skill_id: SkillId(1),
            until_seq: EventSeq(0),
            run_id: "r".into(),
            outcome: "success".into(),
        }));
    }
}
