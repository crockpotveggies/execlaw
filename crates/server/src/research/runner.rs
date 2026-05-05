//! Per-job runner — drives one research job through its phases.
//!
//! C3 ships **plan-only**: the runner picks up a `Pending` row, opens
//! a Card, makes the planner LLM call, persists the plan, transitions
//! to `Planned`, and emits `CardProgressed` (NOT `CardClosed`). The
//! gather phase (C4) and synthesize phase (C5) follow.
//!
//! `phase_gates: plan_only` is the default — `Planned` is the natural
//! pause point; the operator confirms before C4's gather workers
//! fire. With `phase_gates: none` the runner would chain straight
//! into gather; that path lands in C4.
//!
//! Failures are isolated: an LLM error flips the row to `Failed` with
//! the operator-safe error message and emits `CardClosed{Failed}`. A
//! database error during the persistence step is logged and the row
//! is left in `Planning` for the operator to retry / cancel manually
//! — better than half-flipping its state.
//!
//! 2026-04-29.

use crate::cards::{
    CardEmitError, close_card_and_broadcast, open_card_and_broadcast, progress_card_and_broadcast,
};
use crate::events::EventBus;
use crate::research::gather::{GatherCtx, GatherDeps, GatherError, run_gather};
use crate::research::synthesize::{SynthesizeCtx, SynthesizeError, run_synthesize};
use crate::research::workspace::{ResearchWorkspace, WorkspaceError};
use crate::tool_apis_http::HttpWebFetchApi;
use crate::tool_apis_subagent::InferenceSubagentApi;
use execlaw_core::Database;
use execlaw_core::cards::{
    CardAction, CardClosedPayload, CardKind, CardOpenedPayload, CardProgressedPayload, CardState,
};
use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::{
    PhaseGates, PlanStep, ResearchConfigStore, ResearchError, ResearchJobRow, ResearchJobStatus,
    ResearchJobStore, ResearchPlan,
};
use execlaw_core::tool::{SubagentApi, WebFetchApi, WebSearchApi};
use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ResearchRunnerError {
    #[error(transparent)]
    Store(#[from] ResearchError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    CardEmit(#[from] CardEmitError),
    #[error(transparent)]
    Gather(#[from] GatherError),
    #[error(transparent)]
    Synthesize(#[from] SynthesizeError),
    #[error("inference: {0}")]
    Inference(String),
    #[error("planner produced unparseable JSON: {0}")]
    PlannerJson(String),
    #[error("no inference backend wired; cannot run plan phase")]
    NoInference,
}

/// Inputs to a single job-run. The supervisor constructs this and
/// hands it off to `run_job`.
pub struct JobRunCtx {
    pub db: Database,
    pub job_id: ResearchJobId,
    pub workspace: ResearchWorkspace,
    /// Inference client + model id for the planner LLM call. `None`
    /// short-circuits to `Failed` so the supervisor doesn't hold the
    /// row in `Planning` indefinitely.
    pub inference: Option<(Arc<InferenceClient>, String)>,
    /// Bus the runner publishes card lifecycle events to. The
    /// commit-and-broadcast helpers in `crate::cards` write to the
    /// log first, then publish here so a WS-side miss can always
    /// re-project from the durable log.
    pub events: EventBus,
    /// Cancellation token the supervisor minted + registered in
    /// `ResearchSupervisor::cancel_tokens`. The cancel admin
    /// endpoint calls `.cancel()` on the same token, which makes
    /// the gather workers exit cooperatively at their next
    /// checkpoint (between sub-queries / between HTTP calls /
    /// before the subagent call).
    pub cancel: CancellationToken,
}

/// System prompt for the planner LLM call. Asks for a strict JSON
/// shape so we can parse without prose-stripping.
const PLANNER_SYSTEM_PROMPT: &str = "You are the planner for a deep-research job. Given a research question, \
break it into focused sub-queries the gather phase will run in parallel.

If the question is CLEAR enough to plan, reply with EXACTLY a JSON object of this shape:
{\"thesis\":\"<one-paragraph framing>\",\"steps\":[{\"query\":\"<sub-query>\",\"rationale\":\"<one line>\"}, ...]}

If the question is too VAGUE to plan well — missing scope, ambiguous terms, multiple plausible interpretations that would lead to very different results — reply with EXACTLY this shape instead:
{\"clarification\":\"<one or two sentences asking the operator the SINGLE specific question that would let you plan; do not list multiple questions>\"}

Aim for 3-8 sub-queries when planning. Default to planning if the query is workable; only ask for clarification when the spread between plausible interpretations is wide. No prose before or after the JSON. No markdown fences.";

/// Raw JSON the planner emits. The two valid shapes are:
///   * a plan: `thesis` + `steps` populated, `clarification` absent
///   * a clarification request: `clarification` populated, others absent
#[derive(Debug, Deserialize)]
struct PlannerJson {
    #[serde(default)]
    thesis: Option<String>,
    #[serde(default)]
    steps: Option<Vec<PlannerStep>>,
    #[serde(default)]
    clarification: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlannerStep {
    query: String,
    #[serde(default)]
    rationale: Option<String>,
}

/// What `parse_plan` returns. The runner branches on this:
/// `Plan` chains into the gather phase; `NeedsClarification`
/// short-circuits the job to Failed with the operator-visible
/// question on the card summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanOutcome {
    Plan(ResearchPlan),
    NeedsClarification { question: String },
}

/// Run one research job through its phases. Idempotent w.r.t.
/// already-claimed rows (the supervisor calls `claim_next_pending`
/// to gate the entry); not idempotent w.r.t. partial-run recovery
/// — a row left in `Planning` after a crash needs operator action
/// in C3. Auto-recovery lands in a follow-up.
pub async fn run_job(ctx: JobRunCtx) -> Result<ResearchJobRow, ResearchRunnerError> {
    let JobRunCtx {
        db,
        job_id,
        workspace,
        inference,
        events,
        cancel,
    } = ctx;

    // Earliest pre-flight cancel check. The supervisor spawns this
    // task fire-and-forget after claim_next_pending, so the gap
    // between claim and runner pick-up can be milliseconds — long
    // enough for an operator cancel to land. Short-circuit before
    // we provision a workspace dir or hit the JobStore (the
    // status-guarded set_workspace_path would also reject the
    // cancelled row, but with a less-readable NotFound surface).
    if cancel.is_cancelled() {
        let final_row = {
            let db = db.clone();
            let id = job_id.clone();
            tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).get(&id))
                .await
                .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
        };
        return final_row.ok_or_else(|| {
            ResearchRunnerError::Store(ResearchError::NotFound(job_id.as_str().to_owned()))
        });
    }

    // The store calls below are sync; wrap them in spawn_blocking so
    // we don't stall the tokio executor on disk I/O. Cheap clone of
    // Database (it's Arc-wrapped internally).
    let now = chrono::Utc::now().timestamp();

    // Provision the workspace dir + persist the path on the row.
    let dir = {
        let ws = workspace.clone();
        let id = job_id.clone();
        tokio::task::spawn_blocking(move || ws.provision(&id))
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
    };
    let dir_str = dir.to_string_lossy().into_owned();
    {
        let db = db.clone();
        let id = job_id.clone();
        let dir_str = dir_str.clone();
        tokio::task::spawn_blocking(move || {
            ResearchJobStore::new(&db).set_workspace_path(&id, &dir_str, now)
        })
        .await
        .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??;
    }

    // Re-read the row so we have the conversation_id + card_id the
    // supervisor recorded during claim.
    let row = {
        let db = db.clone();
        let id = job_id.clone();
        tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).get(&id))
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
    }
    .ok_or_else(|| ResearchError::NotFound(job_id.as_str().to_owned()))?;

    let card_id = row
        .card_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let conv_id = row.conversation_id.clone();

    // Open the card. CardOpened seeds the projection so the SPA's
    // chat-pane render starts as soon as the supervisor picked the
    // job up.
    open_card_and_broadcast(
        &db,
        &events,
        &conv_id,
        "system",
        &CardOpenedPayload {
            card_id: card_id.clone(),
            kind: CardKind::Research,
            title: truncate_for_card_title(&row.query),
            summary: format!("Researching: {}", truncate_for_card_summary(&row.query)),
            state: Some(CardState::Running),
            details: serde_json::json!({
                "job_id": row.id.as_str(),
                "phase": "Planning",
                "query": row.query,
            }),
            actions: vec![CardAction::OpenDetail {
                href: format!("/research/{}", row.id.as_str()),
            }],
        },
    )?;

    // 2026-05-04 — emit an explicit "Planning" phase update right
    // after the open. Two reasons:
    //   1. On a CLAIMED-RESUME path (clarification → Pending →
    //      claim_next_pending → here), the card was previously left
    //      in `phase: "AwaitingInput"`. The duplicate CardOpened the
    //      runner emitted above resets state but NOT phase (the
    //      SPA's applyEvent for Opened intentionally leaves phase
    //      alone — prior behaviour). This explicit Progressed
    //      flushes the stale phase so the card transitions cleanly
    //      from "Awaiting clarification" → "Planning" instead of
    //      sitting on the stale label until the planner finishes.
    //   2. On a fresh claim, this gives the first-frame card a
    //      meaningful phase label ("Planning") instead of an empty
    //      phase row that flickers in and out.
    progress_card_and_broadcast(
        &db,
        &events,
        &conv_id,
        "system",
        &CardProgressedPayload {
            card_id: card_id.clone(),
            state: Some(CardState::Running),
            progress: Some(0.10),
            phase: Some("Planning".into()),
            details: Some(serde_json::json!({
                "job_id": row.id.as_str(),
                "phase": "Planning",
                "query": row.query,
            })),
            actions: None,
            summary: Some(format!(
                "Planning: {}",
                truncate_for_card_summary(&row.query)
            )),
        },
    )?;

    // Planning phase — single LLM call.
    let (client, model) = match inference {
        Some(i) => i,
        None => {
            mark_failed(
                &db,
                &events,
                &workspace,
                &job_id,
                &conv_id,
                &card_id,
                "no inference backend configured",
            )
            .await;
            return Err(ResearchRunnerError::NoInference);
        }
    };

    // Second pre-flight cancel check (the first is at the top of
    // run_job). Catches a cancel that races the workspace
    // provision + open_card_and_broadcast block above — those are
    // all sub-millisecond, but a cancel observably ordered after
    // the first check is still possible. Skipping here saves the
    // multi-second planner LLM call against a row whose card was
    // already broadcast as Cancelled.
    if cancel.is_cancelled() {
        tracing::info!(
            job_id = job_id.as_str(),
            card_id = %card_id,
            "planner phase: cancellation observed at entry; skipping LLM call",
        );
        // Re-read the row so the caller still gets the (Cancelled)
        // terminal state — the supervisor's spawn_runner_for treats
        // Err returns as failures and logs them, so we want a clean
        // Ok here.
        let final_row = {
            let db = db.clone();
            let id = job_id.clone();
            tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).get(&id))
                .await
                .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
        };
        return final_row.ok_or_else(|| {
            ResearchRunnerError::Store(ResearchError::NotFound(job_id.as_str().to_owned()))
        });
    }

    let plan = match call_planner(&client, &model, &row.query).await {
        Ok(PlanOutcome::Plan(p)) => p,
        Ok(PlanOutcome::NeedsClarification { question }) => {
            // 2026-05-03 (rev 2) — query was too vague to plan. The
            // job pauses in `AwaitingInput` (non-terminal) rather
            // than failing — the agent will see this status, surface
            // the question to the operator in chat, and resume the
            // job via `research_clarify(job_id, answer)`. The card
            // emits a `CardProgressed{phase: AwaitingInput}` (NOT a
            // CardClosed) so the SPA can render the paused state.
            await_input_for_clarification(
                &db, &events, &job_id, &conv_id, &card_id, &row.query, &question,
            )
            .await;
            let final_row = {
                let db = db.clone();
                let id = job_id.clone();
                tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).get(&id))
                    .await
                    .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
            };
            return final_row.ok_or_else(|| {
                ResearchRunnerError::Store(ResearchError::NotFound(job_id.as_str().to_owned()))
            });
        }
        Err(e) => {
            mark_failed(
                &db,
                &events,
                &workspace,
                &job_id,
                &conv_id,
                &card_id,
                &format!("planner failed: {e}"),
            )
            .await;
            return Err(e);
        }
    };

    // Persist plan + flip status. Both the DB row's `plan_json`
    // and the workspace's `plan.json` get the same content; the
    // former is the fast read-path, the latter is the operator-
    // grep view.
    {
        let ws = workspace.clone();
        let id = job_id.clone();
        let plan_for_disk = plan.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || ws.write_plan(&id, &plan_for_disk))
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))?
        {
            tracing::warn!(
                job_id = job_id.as_str(),
                error = %e,
                "writing plan.json to workspace failed; continuing — DB row is the source of truth"
            );
        }
    }
    {
        let db = db.clone();
        let id = job_id.clone();
        let plan_for_db = plan.clone();
        let now = chrono::Utc::now().timestamp();
        tokio::task::spawn_blocking(move || {
            ResearchJobStore::new(&db).set_planned(&id, &plan_for_db, now)
        })
        .await
        .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??;
    }

    // CardProgressed — phase=Planned, plan visible to renderer.
    progress_card_and_broadcast(
        &db,
        &events,
        &conv_id,
        "system",
        &CardProgressedPayload {
            card_id: card_id.clone(),
            state: Some(CardState::Running),
            progress: Some(0.34),
            phase: Some("Planned".into()),
            details: Some(serde_json::json!({
                "job_id": job_id.as_str(),
                "phase": "Planned",
                "query": row.query,
                "plan": plan,
            })),
            actions: None,
            summary: Some("Plan complete.".into()),
        },
    )?;

    // 2026-05-03 — auto-advance: the runner now chains
    // plan → gather → synthesize → complete in one invocation
    // regardless of `config_research.phase_gates`. The
    // operator-facing manual-approval flow is gone; if the
    // research is too vague to plan, the planner returns a
    // clarification question (see `PlanOutcome::NeedsClarification`
    // above) which short-circuits the job to Failed with the
    // question on the card. `phase_gates` is kept on the config
    // row for backward compatibility but the runner ignores it.
    let cfg = {
        let db = db.clone();
        tokio::task::spawn_blocking(move || ResearchConfigStore::new(&db).get())
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
    };
    let phase_deps = PhaseDeps {
        db: db.clone(),
        events: events.clone(),
        workspace: workspace.clone(),
        conversation_id: conv_id.clone(),
        card_id: card_id.clone(),
        inference: client.clone(),
        model: model.clone(),
        cancel: cancel.clone(),
    };
    let notes = run_gather_phase(
        &phase_deps,
        &job_id,
        &plan,
        &cfg,
        /* halt_after_gather = */ false,
    )
    .await?;
    run_synthesize_phase(&phase_deps, &job_id, &row.query, &plan, &notes).await?;

    // Re-read so the caller sees the final row state.
    let final_row = {
        let db = db.clone();
        let id = job_id.clone();
        tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).get(&id))
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
    }
    .ok_or_else(|| ResearchError::NotFound(job_id.as_str().to_owned()))?;
    Ok(final_row)
}

/// Shared inputs the two public phase entry points
/// (`run_gather_phase`, `run_synthesize_phase`) need. Extracted into
/// a struct so the function signatures stay under clippy's
/// too-many-arguments threshold and a future phase addition (e.g.
/// `run_review_phase`) lands by adding one method, not by
/// retrofitting another caller's argument list.
///
/// Cheap to clone — every field is `Arc`-backed (Database is
/// internally Arc, EventBus + ResearchWorkspace are shallow Arc
/// wrappers, InferenceClient is Arc, the rest are plain string /
/// id types).
pub struct PhaseDeps {
    pub db: Database,
    pub events: EventBus,
    pub workspace: ResearchWorkspace,
    pub conversation_id: execlaw_core::ids::ConversationId,
    pub card_id: String,
    pub inference: Arc<InferenceClient>,
    pub model: String,
    /// Cancellation token threaded into the gather phase's per-
    /// worker checkpoints. Defaults to a never-fires token when
    /// the caller has no real token to provide (advance endpoint
    /// for synthesize-only paths, tests, etc.).
    pub cancel: CancellationToken,
}

/// Run the gather phase end-to-end: mark_gathering → run_gather →
/// emit a CardProgressed reflecting the gather completion. When
/// `halt_after_gather` is true (EveryPhase + advance from Planned),
/// the row stays at `Gathering` so the operator's next advance
/// click can fire synthesise; the CardProgressed reports the pause
/// and surfaces an `Approve` action. When false, the caller is
/// expected to chain into `run_synthesize_phase` immediately
/// (None auto-chain + PlanOnly advance from Planned).
///
/// On gather failure the row is marked Failed and CardClosed{Failed}
/// is emitted via `mark_failed` before returning Err.
pub async fn run_gather_phase(
    deps: &PhaseDeps,
    job_id: &ResearchJobId,
    plan: &ResearchPlan,
    cfg: &execlaw_core::research::ResearchConfig,
    halt_after_gather: bool,
) -> Result<Vec<execlaw_core::research::ResearchNote>, ResearchRunnerError> {
    let PhaseDeps {
        db,
        events,
        workspace,
        conversation_id: conv_id,
        card_id,
        inference: client,
        model,
        cancel,
    } = deps;
    // 2026-05-04 — resolve the active provider from
    // config_search_providers instead of hard-coding DDG. The
    // dispatcher fix from rev 9 only touched the agent's
    // `web_search` tool path; deep-research gather still bypassed
    // the resolver and constructed DDG directly here, so the
    // operator's Brave / SearxNG / Tavily / Exa selection in
    // Settings → Search was silently ignored on research jobs.
    // Reported as "DDG anomaly bot-detection" failures despite
    // Brave being the configured default.
    let search: Arc<dyn WebSearchApi> = crate::search_resolver::resolve_active_provider(db);
    let fetch: Arc<dyn WebFetchApi> = Arc::new(HttpWebFetchApi::new());
    let subagent: Arc<dyn SubagentApi> = Arc::new(InferenceSubagentApi::new(
        client.clone(),
        model.to_owned(),
        db.clone(),
        conv_id.clone(),
    ));
    let gather_ctx = GatherCtx {
        db: db.clone(),
        job_id: job_id.clone(),
        conversation_id: conv_id.clone(),
        card_id: card_id.to_owned(),
        workspace: workspace.clone(),
        plan: plan.clone(),
        config: cfg.clone(),
        deps: GatherDeps {
            search,
            fetch,
            subagent: Some(subagent),
        },
        events: events.clone(),
        cancel: cancel.clone(),
    };
    {
        let db = db.clone();
        let id = job_id.clone();
        let now = chrono::Utc::now().timestamp();
        tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).mark_gathering(&id, now))
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??;
    }
    let notes = match run_gather(gather_ctx).await {
        Ok(notes) => notes,
        Err(e) => {
            mark_failed(
                db,
                events,
                workspace,
                job_id,
                conv_id,
                card_id,
                &format!("gather failed: {e}"),
            )
            .await;
            return Err(e.into());
        }
    };
    if halt_after_gather {
        // EveryPhase: leave the row at Gathering, surface an
        // Approve action so the operator can advance to synthesise.
        progress_card_and_broadcast(
            db,
            events,
            conv_id,
            "system",
            &CardProgressedPayload {
                card_id: card_id.to_owned(),
                state: Some(CardState::Running),
                progress: Some(0.85),
                phase: Some("GatherComplete".into()),
                details: None,
                actions: Some(vec![CardAction::OpenDetail {
                    href: format!("/research/{}", job_id.as_str()),
                }]),
                summary: Some("Gather complete. Approve to compose the final report.".into()),
            },
        )?;
    }
    Ok(notes)
}

/// Run the synthesise phase end-to-end: mark_synthesizing →
/// run_synthesize → finish(Complete) → CardClosed{Completed} with
/// the attachment + report markdown inline. On synthesise failure
/// the row is marked Failed and CardClosed{Failed} is emitted via
/// `mark_failed` before returning Err.
pub async fn run_synthesize_phase(
    deps: &PhaseDeps,
    job_id: &ResearchJobId,
    query: &str,
    plan: &ResearchPlan,
    notes: &[execlaw_core::research::ResearchNote],
) -> Result<(), ResearchRunnerError> {
    let PhaseDeps {
        db,
        events,
        workspace,
        conversation_id: conv_id,
        card_id,
        inference: client,
        model,
        cancel,
    } = deps;
    // Pre-flight cancel check. If the operator cancelled during
    // gather (or between phases), the row is already Cancelled —
    // running synthesise would burn a fresh LLM call AND call
    // finish(Complete) which (post-status-guard) is a no-op but
    // still triggers a CardClosed{Completed} broadcast that
    // visually resurrects a job the operator killed. Short-circuit
    // here so cancel actually means cancel.
    if cancel.is_cancelled() {
        tracing::info!(
            card_id = %card_id,
            "synthesize phase: cancellation observed at entry; skipping",
        );
        return Ok(());
    }
    {
        let db = db.clone();
        let id = job_id.clone();
        let now = chrono::Utc::now().timestamp();
        tokio::task::spawn_blocking(move || ResearchJobStore::new(&db).mark_synthesizing(&id, now))
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??;
    }
    progress_card_and_broadcast(
        db,
        events,
        conv_id,
        "system",
        &CardProgressedPayload {
            card_id: card_id.to_owned(),
            state: Some(CardState::Running),
            progress: Some(0.9),
            phase: Some("Synthesizing".into()),
            details: None,
            actions: None,
            summary: Some("Composing the final report.".into()),
        },
    )?;
    let synth_ctx = SynthesizeCtx {
        db: db.clone(),
        job_id: job_id.clone(),
        conversation_id: conv_id.clone(),
        workspace: workspace.clone(),
        query: query.to_owned(),
        plan: plan.clone(),
        notes: notes.to_vec(),
        inference: client.clone(),
        model: model.to_owned(),
    };
    let outcome = match run_synthesize(synth_ctx).await {
        Ok(o) => o,
        Err(e) => {
            mark_failed(
                db,
                events,
                workspace,
                job_id,
                conv_id,
                card_id,
                &format!("synthesize failed: {e}"),
            )
            .await;
            return Err(e.into());
        }
    };
    let finish_landed = {
        let db = db.clone();
        let id = job_id.clone();
        let att = outcome.attachment_id.as_str().to_owned();
        let now = chrono::Utc::now().timestamp();
        tokio::task::spawn_blocking(move || {
            ResearchJobStore::new(&db).finish(
                &id,
                ResearchJobStatus::Complete,
                None,
                Some(&att),
                now,
            )
        })
        .await
        .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
    };
    if !finish_landed {
        // The row was already terminal — almost always because the
        // operator cancelled mid-LLM-call. The cancel handler
        // already broadcast CardClosed{Cancelled}; broadcasting
        // CardClosed{Completed} here would visually resurrect the
        // job. Skip the close-card emit and exit silently. The
        // workspace write + attachment row stay (harmless;
        // operators can still read the report if they want).
        tracing::info!(
            job_id = job_id.as_str(),
            "synthesize completed but row was already terminal (likely cancelled mid-LLM); skipping CardClosed{{Completed}} broadcast",
        );
        return Ok(());
    }
    // 2026-05-03 (rev 3): we used to inline the entire report
    // markdown into `details.report_markdown`. The SPA dumped it
    // into a `<pre>` block below the plan tree which bloated the
    // chat pane on any non-trivial report. The PDF deliverable now
    // flows through a separate Attachment card the SPA renders as
    // a download chip (emitted below). The research card still
    // carries `attachment_id` for back-compat and the
    // /research/<job_id> page can pull the markdown from the
    // workspace for operators who want to read inline.
    close_card_and_broadcast(
        db,
        events,
        conv_id,
        "system",
        &CardClosedPayload {
            card_id: card_id.to_owned(),
            state: CardState::Completed,
            summary: "Research complete. PDF report attached below.".into(),
            // attachment_id duplicated into details so the SPA's
            // Download button can find it via either route — the
            // top-level CardClosedPayload field OR the details
            // JSON. Two paths because we've seen the top-level
            // field occasionally not surface in some live-WS edge
            // cases; details.attachment_id always travels with
            // the card's serialized JSON because the projection
            // re-serializes details verbatim.
            details: Some(serde_json::json!({
                "job_id": job_id.as_str(),
                "phase": "Complete",
                "report_url": format!("/research/{}", job_id.as_str()),
                "attachment_id": outcome.attachment_id.as_str(),
            })),
            attachment_id: Some(outcome.attachment_id.as_str().to_owned()),
            error: None,
        },
    )?;

    // 2026-05-04 (rev 9): the separate Attachment card emit is
    // gone. The PDF deliverable now renders as an inline Download
    // button on the ResearchCard itself (see web/src/cards/
    // ResearchCard.tsx). Two reasons:
    //   * The dual-card layout (research card + chip) read as
    //     visually duplicative — "research complete" message
    //     followed by a separate "here's a file" chip created
    //     a two-step that felt sloppy.
    //   * The chip's render diverged between live (CardOpened/
    //     Closed) and replayed (cardStore hydration) paths,
    //     making refresh feel buggy.
    // The download button keys on attachment_id (already on the
    // ResearchCard's CardClosedPayload above) so live + replayed
    // render through the same code path.
    //
    // The agent's send_attachment tool stays available for
    // future flows that DO want a separate chip (multi-
    // attachment composition, ad-hoc file delivery from the
    // agent, etc.). Just not this code path.
    Ok(())
}

async fn call_planner(
    client: &InferenceClient,
    model: &str,
    query: &str,
) -> Result<PlanOutcome, ResearchRunnerError> {
    let req = ChatRequest {
        model: ModelId(model.to_owned()),
        messages: vec![
            ChatMessage::system(PLANNER_SYSTEM_PROMPT),
            ChatMessage::user(query.to_owned()),
        ],
        max_tokens: Some(1024),
        temperature: Some(0.2),
        stream: false,
        tools: None,
        // Adapter applies per-family kwargs (e.g. enable_thinking
        // for Qwen3); leave None here so the adapter's choice wins.
        chat_template_kwargs: None,
    };
    let adapter =
        execlaw_model_adapter::adapter_for(execlaw_model_adapter::ModelFamily::detect(model));
    let adapted = adapter
        .chat(
            client,
            req,
            execlaw_model_adapter::OutputHint::StructuredJson,
        )
        .await
        .map_err(|e| ResearchRunnerError::Inference(e.to_string()))?;
    parse_plan(&adapted.content)
}

/// Best-effort parser. The planner is told to reply with strict JSON,
/// but real LLMs sometimes wrap the JSON in a ```json fence, prepend
/// a "Thinking Process:" preamble, or include `{` characters inside
/// thinking text BEFORE the real object. We try a sequence of
/// extraction strategies and take the first that parses.
///
/// Two valid output shapes (see `PLANNER_SYSTEM_PROMPT`):
///   * Plan: `thesis` + non-empty `steps` → `PlanOutcome::Plan`
///   * Clarification: `clarification` populated → `PlanOutcome::NeedsClarification`
///
/// Clarification wins if both are present (defensive — the prompt
/// asks for one or the other, but we'd rather honor an
/// operator-visible question than silently run a half-baked plan).
fn parse_plan(raw: &str) -> Result<PlanOutcome, ResearchRunnerError> {
    let candidate = match extract_planner_json(raw) {
        Some(c) => c,
        None => {
            return Err(ResearchRunnerError::PlannerJson(format!(
                "no parseable JSON object found — text was: {}",
                truncate_for_error(raw, 400)
            )));
        }
    };
    let parsed: PlannerJson = serde_json::from_str(&candidate).map_err(|e| {
        ResearchRunnerError::PlannerJson(format!(
            "{e} — text was: {}",
            truncate_for_error(&candidate, 400)
        ))
    })?;

    // Clarification path takes precedence.
    if let Some(q) = parsed
        .clarification
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(PlanOutcome::NeedsClarification {
            question: q.to_string(),
        });
    }

    let raw_steps = parsed.steps.unwrap_or_default();
    let steps: Vec<PlanStep> = raw_steps
        .into_iter()
        .filter(|s| !s.query.trim().is_empty())
        .map(|s| PlanStep {
            query: s.query.trim().to_owned(),
            rationale: s.rationale.map(|r| r.trim().to_owned()),
        })
        .collect();
    if steps.is_empty() {
        return Err(ResearchRunnerError::PlannerJson(
            "planner produced zero usable sub-queries".into(),
        ));
    }
    let thesis = parsed
        .thesis
        .map(|t| t.trim().to_owned())
        .unwrap_or_default();
    Ok(PlanOutcome::Plan(ResearchPlan { thesis, steps }))
}

/// Find a parseable JSON object inside the planner's reply.
///
/// Strategy, in order:
///   1. Trim whitespace; if the result starts with `{`, return it.
///   2. Strip a ```json (or bare ```) fence + trailing fence.
///   3. Strip well-known thinking preambles (`<think>...</think>` and
///      Qwen3.5's literal "Thinking Process:" block).
///   4. Walk every `{` position in the (possibly-stripped) text and
///      return the substring from each one to its matching `}` —
///      first one that parses as the planner's expected shape wins.
///
/// Returns `None` only when no balanced `{...}` substring parses.
pub(crate) fn extract_planner_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') && serde_json::from_str::<PlannerJson>(trimmed).is_ok() {
        return Some(trimmed.to_owned());
    }
    // Strip ```json fence pair.
    let unfenced = strip_code_fences(trimmed);
    let unfenced_trim = unfenced.trim();
    if unfenced_trim.starts_with('{') && serde_json::from_str::<PlannerJson>(unfenced_trim).is_ok()
    {
        return Some(unfenced_trim.to_owned());
    }
    // Strip thinking blocks.
    let depreamble = strip_thinking_preamble(unfenced_trim);
    let depreamble_trim = depreamble.trim();
    if depreamble_trim.starts_with('{')
        && serde_json::from_str::<PlannerJson>(depreamble_trim).is_ok()
    {
        return Some(depreamble_trim.to_owned());
    }
    // Brace-walk: try every `{` position. First one with a balanced
    // matching `}` whose substring parses as PlannerJson wins.
    for (pos, _) in depreamble_trim.match_indices('{') {
        if let Some(end) = matching_brace_end(depreamble_trim, pos) {
            let slice = &depreamble_trim[pos..=end];
            if serde_json::from_str::<PlannerJson>(slice).is_ok() {
                return Some(slice.to_owned());
            }
        }
    }
    None
}

fn strip_code_fences(s: &str) -> String {
    let mut working = s.to_owned();
    if let Some(rest) = working.strip_prefix("```json") {
        working = rest.trim_start().to_owned();
    } else if let Some(rest) = working.strip_prefix("```") {
        working = rest.trim_start().to_owned();
    }
    if let Some(end) = working.rfind("```") {
        working.truncate(end);
    }
    working
}

/// Strip Qwen3.5's "Thinking Process:" prefix and any `<think>...</think>`
/// block. Both shapes have shown up in the wild even with
/// `enable_thinking: false` set on certain backend versions.
fn strip_thinking_preamble(s: &str) -> String {
    let mut working = s.to_owned();
    // <think>...</think> wrapper (xml-style). Drop everything up
    // to and including </think>.
    if let Some(close) = working.find("</think>") {
        working = working[close + "</think>".len()..].to_string();
    }
    // Literal "Thinking Process:" block — Qwen3.5 sometimes emits
    // this in plain prose. The block typically ends at the start of
    // the actual structured output. Heuristic: if the text starts
    // with "Thinking Process:" and a JSON object follows, slice from
    // the first `{`. The brace-walk fallback in `extract_planner_json`
    // handles the case where a `{` appears inside the thinking text
    // itself (e.g. an example shown in a numbered list).
    let lower = working.to_ascii_lowercase();
    if lower.starts_with("thinking process:") || lower.contains("\nthinking process:") {
        if let Some(first_brace) = working.find('{') {
            working = working[first_brace..].to_string();
        }
    }
    working
}

/// Find the index of the `}` that matches the `{` at `start`.
/// Tracks string literals + backslash escapes so a `}` inside a
/// quoted string doesn't count.
fn matching_brace_end(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes[start], b'{');
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate_for_error(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("…[truncated]");
    out
}

async fn mark_failed(
    db: &Database,
    events: &EventBus,
    workspace: &ResearchWorkspace,
    job_id: &ResearchJobId,
    conv_id: &execlaw_core::ids::ConversationId,
    card_id: &str,
    reason: &str,
) {
    let _ = workspace; // future: leave a `failure.json` breadcrumb
    let now = chrono::Utc::now().timestamp();
    let id = job_id.clone();
    let db_for_task = db.clone();
    let reason_owned = reason.to_owned();
    let finish_res = tokio::task::spawn_blocking(move || {
        ResearchJobStore::new(&db_for_task).finish(
            &id,
            ResearchJobStatus::Failed,
            Some(&reason_owned),
            None,
            now,
        )
    })
    .await;
    let landed = match &finish_res {
        Ok(Ok(b)) => *b,
        Ok(Err(e)) => {
            tracing::warn!(
                job_id = job_id.as_str(),
                error = %e,
                "marking job Failed in DB hit an error; row may be stuck in Planning",
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                job_id = job_id.as_str(),
                error = %e,
                "marking job Failed in DB join error",
            );
            false
        }
    };
    if !landed {
        // Row was already terminal — almost always a cancel that
        // raced this failure path. The cancel handler already
        // broadcast CardClosed{Cancelled}; emitting CardClosed{Failed}
        // here would visually re-open the row in a different state
        // for SPA subscribers.
        tracing::info!(
            job_id = job_id.as_str(),
            "mark_failed: row already terminal (likely cancelled); skipping CardClosed{{Failed}} broadcast",
        );
        return;
    }
    let close = close_card_and_broadcast(
        db,
        events,
        conv_id,
        "system",
        &CardClosedPayload {
            card_id: card_id.to_owned(),
            state: CardState::Failed,
            summary: format!("Research failed: {reason}"),
            details: None,
            attachment_id: None,
            error: Some(reason.to_owned()),
        },
    );
    if let Err(e) = close {
        tracing::warn!(
            job_id = job_id.as_str(),
            error = %e,
            "emitting CardClosed for failed job hit an error",
        );
    }
}

/// Pause the job in `AwaitingInput` and broadcast a
/// `CardProgressed{phase: AwaitingInput}` so the SPA can render the
/// "agent will ask the operator about this" state. The agent (per
/// project-locked-decisions 2026-04-23: agent is the primary
/// interface) sees the same state via `research_status` and surfaces
/// the question in chat.
///
/// Mirrors `mark_failed`'s race-safety: a cancel that beats us to
/// the row leaves it terminal; the `set_awaiting_input` UPDATE
/// guards on `status NOT IN (terminal)`, returns false when the
/// guard fires, and we then skip the CardProgressed broadcast.
async fn await_input_for_clarification(
    db: &Database,
    events: &EventBus,
    job_id: &ResearchJobId,
    conv_id: &execlaw_core::ids::ConversationId,
    card_id: &str,
    original_query: &str,
    question: &str,
) {
    let now = chrono::Utc::now().timestamp();
    let id = job_id.clone();
    let db_for_task = db.clone();
    let q_owned = question.to_owned();
    let res = tokio::task::spawn_blocking(move || {
        ResearchJobStore::new(&db_for_task).set_awaiting_input(&id, &q_owned, now)
    })
    .await;
    let landed = match &res {
        Ok(Ok(b)) => *b,
        Ok(Err(e)) => {
            tracing::warn!(
                job_id = job_id.as_str(),
                error = %e,
                "set_awaiting_input hit a DB error; row may be stuck in Planning",
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                job_id = job_id.as_str(),
                error = %e,
                "set_awaiting_input join error",
            );
            false
        }
    };
    if !landed {
        // Row was already terminal (cancel race). Don't broadcast a
        // CardProgressed that contradicts the prior CardClosed.
        tracing::info!(
            job_id = job_id.as_str(),
            "await_input: row already terminal (likely cancelled); skipping CardProgressed{{AwaitingInput}} broadcast",
        );
        return;
    }
    let progressed = progress_card_and_broadcast(
        db,
        events,
        conv_id,
        "system",
        &CardProgressedPayload {
            card_id: card_id.to_owned(),
            state: Some(CardState::Running),
            // Plan phase is ~1/3 of the work; sit at that progress
            // value so the bar doesn't visually jump backwards if
            // the operator clarifies and we re-enter Planning.
            progress: Some(0.33),
            phase: Some("AwaitingInput".into()),
            details: Some(serde_json::json!({
                "job_id": job_id.as_str(),
                "phase": "AwaitingInput",
                "query": original_query,
                "clarification_question": question,
            })),
            actions: None,
            summary: Some(
                "Awaiting clarification — the agent will relay the planner's question.".into(),
            ),
        },
    );
    if let Err(e) = progressed {
        tracing::warn!(
            job_id = job_id.as_str(),
            error = %e,
            "emitting CardProgressed{{AwaitingInput}} hit an error",
        );
    }
    // Sister event for the server-side clarification listener,
    // which fires a follow-up agent turn so the agent relays the
    // question to the user in chat. Distinct from CardProgressed
    // so the listener can subscribe with a narrow filter and so
    // SPA card-store doesn't double-process.
    events.publish(crate::events::UiEvent::ResearchAwaitingInput {
        conversation_id: conv_id.as_str().to_owned(),
        job_id: job_id.as_str().to_owned(),
        question: question.to_owned(),
    });
}

fn truncate_for_card_title(s: &str) -> String {
    truncate_to(s, 80)
}
fn truncate_for_card_summary(s: &str) -> String {
    truncate_to(s, 140)
}

fn truncate_to(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    let mut buf: String = trimmed.chars().take(max - 1).collect();
    buf.push('…');
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_plan(o: PlanOutcome) -> ResearchPlan {
        match o {
            PlanOutcome::Plan(p) => p,
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn parse_plan_accepts_strict_json() {
        let plan = unwrap_plan(
            parse_plan(r#"{"thesis":"t","steps":[{"query":"q1","rationale":"r1"}]}"#).unwrap(),
        );
        assert_eq!(plan.thesis, "t");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].query, "q1");
    }

    #[test]
    fn parse_plan_strips_markdown_fences() {
        let plan = unwrap_plan(
            parse_plan("```json\n{\"thesis\":\"t\",\"steps\":[{\"query\":\"q1\"}]}\n```").unwrap(),
        );
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn parse_plan_strips_leading_prose() {
        let plan = unwrap_plan(
            parse_plan("Sure! Here's the plan: {\"thesis\":\"t\",\"steps\":[{\"query\":\"q1\"}]}")
                .unwrap(),
        );
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn parse_plan_rejects_zero_steps() {
        let err = parse_plan(r#"{"thesis":"t","steps":[]}"#).unwrap_err();
        assert!(matches!(err, ResearchRunnerError::PlannerJson(_)));
    }

    #[test]
    fn parse_plan_rejects_garbage() {
        let err = parse_plan("totally not json").unwrap_err();
        assert!(matches!(err, ResearchRunnerError::PlannerJson(_)));
    }

    #[test]
    fn parse_plan_returns_clarification_when_present() {
        let raw = r#"{"clarification":"Do you mean US zones 4-10 or international zones?"}"#;
        let outcome = parse_plan(raw).unwrap();
        match outcome {
            PlanOutcome::NeedsClarification { question } => {
                assert!(question.contains("zones"));
            }
            other => panic!("expected NeedsClarification, got {other:?}"),
        }
    }

    #[test]
    fn parse_plan_clarification_wins_when_both_shapes_present() {
        // Defensive: if the model emits both clarification AND a
        // half-baked plan, honor the clarification (operator-visible
        // question is safer than running with a guess).
        let raw = r#"{"clarification":"Specify the region.","thesis":"x","steps":[{"query":"q"}]}"#;
        match parse_plan(raw).unwrap() {
            PlanOutcome::NeedsClarification { question } => {
                assert!(question.contains("region"));
            }
            other => panic!("expected NeedsClarification, got {other:?}"),
        }
    }

    #[test]
    fn parse_plan_clarification_in_thinking_preamble_still_extracted() {
        // The brace-walker should find the clarification object even
        // behind a Qwen "Thinking Process:" preamble.
        let raw = "Thinking Process:\n1. The query is broad.\n\n{\"clarification\":\"Region?\"}";
        match parse_plan(raw).unwrap() {
            PlanOutcome::NeedsClarification { question } => {
                assert_eq!(question, "Region?");
            }
            other => panic!("expected NeedsClarification, got {other:?}"),
        }
    }

    #[test]
    fn parse_plan_filters_empty_step_queries() {
        let plan = unwrap_plan(
            parse_plan(r#"{"thesis":"t","steps":[{"query":"   "},{"query":"real"}]}"#).unwrap(),
        );
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].query, "real");
    }

    /// Regression: Qwen3.5 sometimes ignores `enable_thinking: false`
    /// and emits its native "Thinking Process:" preamble in front of
    /// the JSON. The parser used to truncate at the first `}` it saw
    /// — which sometimes appeared in the thinking text itself —
    /// producing a parse error.
    #[test]
    fn parse_plan_skips_thinking_process_preamble_with_braces_in_prose() {
        let raw = r#"Thinking Process:
1. **Analyze the Request:**
   * Output Format: EXACTLY a JSON object with keys "thesis" and "steps". Each step must have "query" and "rationale".
   * Example placeholder: { 'thesis': '...', 'steps': [...] }  ← single quotes so this isn't valid JSON
2. **Build the plan.**

{"thesis": "ground covers vary by zone", "steps": [{"query": "evergreen ground cover zone 4", "rationale": "cold-hardy options"}, {"query": "evergreen ground cover zone 10", "rationale": "heat-tolerant options"}]}"#;
        let plan = unwrap_plan(parse_plan(raw).unwrap());
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.thesis.contains("zone"));
    }

    #[test]
    fn parse_plan_handles_xml_think_block_wrapper() {
        let raw = r#"<think>The user wants a research plan. I'll structure it as JSON with thesis and steps.</think>
{"thesis": "t", "steps": [{"query": "q"}]}"#;
        let plan = unwrap_plan(parse_plan(raw).unwrap());
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn parse_plan_picks_first_valid_json_when_brace_walking() {
        // Multiple `{` candidates; only one parses.
        let raw = r#"prelude { not really json } more text {"thesis":"t","steps":[{"query":"q"}]} trailing"#;
        let plan = unwrap_plan(parse_plan(raw).unwrap());
        assert_eq!(plan.thesis, "t");
    }

    #[test]
    fn parse_plan_error_message_truncates_long_inputs() {
        // No JSON at all — error should contain a truncated preview,
        // not echo back kilobytes of model output.
        let raw = format!("Thinking Process:\n{}", "a".repeat(5000));
        let err = parse_plan(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.len() < 1000,
            "error message must be bounded; got {} chars",
            msg.len()
        );
        assert!(msg.contains("no parseable JSON") || msg.contains("text was:"));
    }

    #[test]
    fn matching_brace_end_handles_strings_with_escaped_quotes() {
        // The `}` inside the string literal must NOT close the
        // outer object early.
        let s = r#"{"thesis":"has \"quoted\" } content","steps":[{"query":"q"}]}"#;
        let end = matching_brace_end(s, 0).unwrap();
        // The matching brace is the LAST char (after the inner step
        // object closes too). We just need the substring to parse.
        let slice = &s[0..=end];
        assert!(serde_json::from_str::<PlannerJson>(slice).is_ok());
    }

    #[test]
    fn matching_brace_end_handles_nested_braces() {
        let s = r#"{"a":{"b":{"c":1}},"d":2}"#;
        let end = matching_brace_end(s, 0).unwrap();
        assert_eq!(end, s.len() - 1);
    }

    #[test]
    fn truncate_passes_through_short_strings() {
        assert_eq!(truncate_to("short", 80), "short");
    }

    #[test]
    fn truncate_caps_long_strings_with_ellipsis() {
        let long = "x".repeat(200);
        let cut = truncate_to(&long, 80);
        assert!(cut.chars().count() <= 80);
        assert!(cut.ends_with('…'));
    }

    /// Adversarial: a `run_synthesize_phase` invocation whose
    /// `PhaseDeps.cancel` is already cancelled at entry must NOT
    /// touch the DB / make the LLM call / emit CardClosed. The
    /// inference client points at a closed port — if we accidentally
    /// reach the LLM call this test would fail with a connection
    /// error rather than a clean Ok(()). The pre-flight check is
    /// what prevents wasted token spend after an operator cancel.
    #[tokio::test]
    async fn run_synthesize_phase_early_exits_on_cancelled_token() {
        use execlaw_core::Database;
        use execlaw_core::cards::{CardKind, CardOpenedPayload, CardState};
        use execlaw_core::conversation::{
            ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
        };
        use execlaw_core::db::DbConfig;
        use execlaw_core::ids::{ConversationId, EventSeq};
        use execlaw_core::migrations::MigrationRunner;
        use execlaw_core::research::{PlanStep, ResearchJobStatus, ResearchJobStore, ResearchPlan};

        // In-memory DB + a seeded job in Gathering status.
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let cid = ConversationId::from("conv-cancel-synth");
        ConversationStore::new(&db)
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
        let id = ResearchJobId::new();
        let store = ResearchJobStore::new(&db);
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("card-1", 110).unwrap();
        let plan = ResearchPlan {
            thesis: "t".into(),
            steps: vec![PlanStep {
                query: "q1".into(),
                rationale: None,
            }],
        };
        store.set_planned(&id, &plan, 120).unwrap();
        store.mark_gathering(&id, 130).unwrap();
        // Operator cancel — flips row to Cancelled.
        store.cancel_active(&id, Some("operator"), 140).unwrap();

        // Open the card so the close path has a row to project from
        // if we accidentally fall through.
        let tmp = tempfile::tempdir().unwrap();
        crate::cards::open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-1".into(),
                kind: CardKind::Research,
                title: "t".into(),
                summary: "s".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();

        // PhaseDeps with a CANCELLED token. The inference URL is
        // closed (127.0.0.1:0) so any actual LLM call would error
        // out distinguishably.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let deps = PhaseDeps {
            db: db.clone(),
            events: EventBus::new(),
            workspace: ResearchWorkspace::new(tmp.path()),
            conversation_id: cid.clone(),
            card_id: "card-1".into(),
            inference: Arc::new(InferenceClient::new("http://127.0.0.1:0/v1")),
            model: "test-model".into(),
            cancel,
        };

        // Must return Ok(()) cleanly without making the LLM call,
        // without overwriting the Cancelled row, without emitting
        // CardClosed{Completed}.
        run_synthesize_phase(&deps, &id, "q", &plan, &[])
            .await
            .unwrap();

        // Row stays Cancelled.
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert_eq!(row.finished_at, Some(140));
        assert_eq!(row.error.as_deref(), Some("operator"));
        assert!(row.attachment_id.is_none());
    }

    /// Adversarial race: the cancel token's pre-flight check passes
    /// (token not yet cancelled), the synthesize LLM call runs to
    /// completion, but the operator cancels DURING the LLM call so
    /// by the time we hit `finish(Complete)` the row is already
    /// Cancelled. The status guard makes finish a no-op (returns
    /// false), but without the bool-handling fix the runner would
    /// still broadcast CardClosed{Completed} — visually resurrecting
    /// the job. Pre-flips the row to Cancelled in the DB while
    /// keeping the cancel token clear, then runs the synthesize
    /// path against a working mock LLM and asserts NO
    /// CardClosed{Completed} event fires.
    #[tokio::test]
    async fn run_synthesize_phase_skips_close_card_when_row_already_cancelled() {
        use execlaw_core::Database;
        use execlaw_core::cards::{CardKind, CardOpenedPayload, CardState};
        use execlaw_core::conversation::{
            ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
        };
        use execlaw_core::db::DbConfig;
        use execlaw_core::ids::{ConversationId, EventSeq};
        use execlaw_core::migrations::MigrationRunner;
        use execlaw_core::research::{
            PlanStep, ResearchJobStatus, ResearchJobStore, ResearchNote, ResearchPlan,
            SubQueryState,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Spin up a working mock LLM listener so the synthesize call
        // succeeds end-to-end (we want to reach `finish(Complete)`,
        // not bail out on inference error).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "id": "syn-1",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "# Final\n\nDone."},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let cid = ConversationId::from("conv-cancel-race");
        ConversationStore::new(&db)
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
        let id = ResearchJobId::new();
        let store = ResearchJobStore::new(&db);
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("card-race", 110).unwrap();
        let plan = ResearchPlan {
            thesis: "t".into(),
            steps: vec![PlanStep {
                query: "q1".into(),
                rationale: None,
            }],
        };
        store.set_planned(&id, &plan, 120).unwrap();
        store.mark_gathering(&id, 130).unwrap();
        // Simulate the race: the operator cancel landed AFTER the
        // pre-flight token check but before finish(Complete). Flip
        // the row to Cancelled in the DB while leaving the local
        // cancel token clear.
        store.cancel_active(&id, Some("race"), 140).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        crate::cards::open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-race".into(),
                kind: CardKind::Research,
                title: "t".into(),
                summary: "s".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();

        // Subscribe BEFORE running synthesize so we capture every
        // event the phase emits. Token is intentionally NOT
        // cancelled so the pre-flight check at the top of
        // run_synthesize_phase passes.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let cancel = CancellationToken::new();
        let deps = PhaseDeps {
            db: db.clone(),
            events: bus,
            workspace: ResearchWorkspace::new(tmp.path()),
            conversation_id: cid.clone(),
            card_id: "card-race".into(),
            inference: Arc::new(InferenceClient::new(format!("http://{addr}/v1"))),
            model: "test-model".into(),
            cancel,
        };

        let notes = vec![ResearchNote {
            index: 0,
            sub_query: "q1".into(),
            state: SubQueryState::Done,
            excerpt: "did the thing".into(),
            sources: Vec::new(),
            tokens_used: Some(10),
            error: None,
        }];

        run_synthesize_phase(&deps, &id, "q", &plan, &notes)
            .await
            .unwrap();

        // Drain the event bus and assert NO CardClosed{Completed}
        // event fired. In-progress CardProgressed events are fine
        // (the phase emits one for "Synthesizing"); they just don't
        // resurrect a Cancelled row.
        loop {
            match rx.try_recv() {
                Ok(crate::events::UiEvent::CardClosed { state, .. }) => {
                    panic!(
                        "unexpected CardClosed (state={state}) — synthesize must skip the broadcast when the row is already terminal"
                    );
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        // Row stays Cancelled — finish(Complete) was rejected by the
        // status guard and didn't overwrite the operator's note.
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert_eq!(row.error.as_deref(), Some("race"));
        assert_eq!(row.finished_at, Some(140));
    }

    /// Same race, opposite path: gather fails AFTER an operator
    /// cancel landed. mark_failed's finish(Failed) hits the status
    /// guard and returns false; without the bool-handling fix the
    /// helper would still emit CardClosed{Failed}, flipping a card
    /// the cancel handler already closed as Cancelled.
    #[tokio::test]
    async fn mark_failed_skips_close_card_when_row_already_cancelled() {
        use execlaw_core::Database;
        use execlaw_core::cards::{CardKind, CardOpenedPayload, CardState};
        use execlaw_core::conversation::{
            ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
        };
        use execlaw_core::db::DbConfig;
        use execlaw_core::ids::{ConversationId, EventSeq};
        use execlaw_core::migrations::MigrationRunner;
        use execlaw_core::research::{ResearchJobStatus, ResearchJobStore};

        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let cid = ConversationId::from("conv-mark-failed-race");
        ConversationStore::new(&db)
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
        let id = ResearchJobId::new();
        let store = ResearchJobStore::new(&db);
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("card-mf", 110).unwrap();
        // Pre-cancel — simulates the race where the operator cancel
        // landed before this gather-failure path runs mark_failed.
        store
            .cancel_active(&id, Some("operator note"), 120)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        crate::cards::open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-mf".into(),
                kind: CardKind::Research,
                title: "t".into(),
                summary: "s".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let workspace = ResearchWorkspace::new(tmp.path());

        mark_failed(&db, &bus, &workspace, &id, &cid, "card-mf", "fetch error").await;

        // No CardClosed event of any kind should have fired.
        loop {
            match rx.try_recv() {
                Ok(crate::events::UiEvent::CardClosed { state, .. }) => {
                    panic!(
                        "unexpected CardClosed (state={state}) — mark_failed must skip when row already terminal"
                    );
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        // Row stays Cancelled with the operator's note intact.
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert_eq!(row.error.as_deref(), Some("operator note"));
    }

    /// Adversarial: an operator cancel that lands BEFORE run_job
    /// reaches the planner LLM call must short-circuit — the
    /// supervisor's tick spawns the runner fire-and-forget, so an
    /// immediate cancel can land in that window. Without the
    /// pre-flight check the planner would still run the LLM call
    /// (multi-second token spend) for a row whose card was already
    /// broadcast as Cancelled.
    ///
    /// Wires the inference client at a closed port so a missed
    /// pre-flight check would surface as a connect error instead of
    /// a clean Ok.
    #[tokio::test]
    async fn run_job_short_circuits_when_cancelled_before_planner_call() {
        use crate::events::EventBus;
        use crate::research::workspace::ResearchWorkspace;
        use execlaw_core::Database;
        use execlaw_core::cards::{CardKind, CardOpenedPayload, CardState};
        use execlaw_core::conversation::{
            ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
        };
        use execlaw_core::db::DbConfig;
        use execlaw_core::ids::{ConversationId, EventSeq};
        use execlaw_core::migrations::MigrationRunner;
        use execlaw_core::research::{ResearchJobStatus, ResearchJobStore};

        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let cid = ConversationId::from("conv-pre-planner-cancel");
        ConversationStore::new(&db)
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
        let id = ResearchJobId::new();
        let store = ResearchJobStore::new(&db);
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        // Mirror the supervisor: claim the row + cancel before the
        // runner picks it up.
        store.claim_next_pending("card-pre-cancel", 110).unwrap();
        store.cancel_active(&id, Some("operator"), 120).unwrap();

        // Open the card so the early-exit's row re-read finds an
        // intact projection.
        crate::cards::open_card(
            &db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: "card-pre-cancel".into(),
                kind: CardKind::Research,
                title: "t".into(),
                summary: "s".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();

        // Inference client at port 0 — any LLM call would error
        // distinguishably. The pre-flight check must fire before
        // we reach this address.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = JobRunCtx {
            db: db.clone(),
            job_id: id.clone(),
            workspace: ResearchWorkspace::new(tmp.path()),
            inference: Some((
                Arc::new(InferenceClient::new("http://127.0.0.1:0/v1")),
                "test-model".into(),
            )),
            events: EventBus::new(),
            cancel,
        };

        let row = run_job(ctx).await.expect("must short-circuit cleanly");
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert_eq!(row.error.as_deref(), Some("operator"));
        assert!(row.attachment_id.is_none());
    }
}
