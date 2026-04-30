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

use crate::cards::{CardEmitError, close_card, open_card, progress_card};
use crate::research::workspace::{ResearchWorkspace, WorkspaceError};
use execlaw_core::Database;
use execlaw_core::cards::{
    CardAction, CardClosedPayload, CardKind, CardOpenedPayload, CardProgressedPayload, CardState,
};
use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::{
    PhaseGates, PlanStep, ResearchConfigStore, ResearchError, ResearchJobRow, ResearchJobStatus,
    ResearchJobStore, ResearchPlan,
};
use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ResearchRunnerError {
    #[error(transparent)]
    Store(#[from] ResearchError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    CardEmit(#[from] CardEmitError),
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
}

/// System prompt for the planner LLM call. Asks for a strict JSON
/// shape so we can parse without prose-stripping.
const PLANNER_SYSTEM_PROMPT: &str = "You are the planner for a deep-research job. Given a research question, \
break it into focused sub-queries the gather phase will run in parallel. \
Reply with EXACTLY a JSON object of the shape: \
{\"thesis\":\"<one-paragraph framing>\",\"steps\":[{\"query\":\"<sub-query>\",\"rationale\":\"<one line>\"}, ...]} \
No prose before or after the JSON. No markdown fences. Aim for 3-8 sub-queries.";

#[derive(Debug, Deserialize)]
struct PlannerJson {
    thesis: String,
    steps: Vec<PlannerStep>,
}

#[derive(Debug, Deserialize)]
struct PlannerStep {
    query: String,
    #[serde(default)]
    rationale: Option<String>,
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
    } = ctx;

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
    open_card(
        &db,
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

    // Planning phase — single LLM call.
    let (client, model) = match inference {
        Some(i) => i,
        None => {
            mark_failed(
                &db,
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

    let plan = match call_planner(&client, &model, &row.query).await {
        Ok(p) => p,
        Err(e) => {
            mark_failed(
                &db,
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
    progress_card(
        &db,
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

    // Phase-gate decision. C3 only knows `plan_only` (default) and
    // `none`; `every_phase` lands in C6 alongside the approval-flow
    // wiring. With `none`, gather would fire next — but gather
    // itself isn't implemented until C4, so we currently log a
    // diagnostic and stop at `Planned` either way.
    let cfg = {
        let db = db.clone();
        tokio::task::spawn_blocking(move || ResearchConfigStore::new(&db).get())
            .await
            .map_err(|e| ResearchRunnerError::Inference(format!("join: {e}")))??
    };
    if !matches!(cfg.phase_gates, PhaseGates::PlanOnly) {
        tracing::info!(
            job_id = job_id.as_str(),
            gates = cfg.phase_gates.as_str(),
            "phase_gates != plan_only — gather phase lands in C4; stopping at Planned"
        );
    }

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

async fn call_planner(
    client: &InferenceClient,
    model: &str,
    query: &str,
) -> Result<ResearchPlan, ResearchRunnerError> {
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
        chat_template_kwargs: None,
    };
    let resp = client
        .chat_completions(&req)
        .await
        .map_err(|e| ResearchRunnerError::Inference(e.to_string()))?;
    let text = resp
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    parse_plan(&text)
}

/// Best-effort parser. The planner is told to reply with strict JSON,
/// but real LLMs sometimes wrap the JSON in a ```json fence or add a
/// preamble. Strip those before deserialising.
fn parse_plan(raw: &str) -> Result<ResearchPlan, ResearchRunnerError> {
    let candidate = strip_fences_and_prose(raw);
    let parsed: PlannerJson = serde_json::from_str(&candidate)
        .map_err(|e| ResearchRunnerError::PlannerJson(format!("{e} — text was: {candidate}")))?;
    let steps: Vec<PlanStep> = parsed
        .steps
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
    Ok(ResearchPlan {
        thesis: parsed.thesis.trim().to_owned(),
        steps,
    })
}

fn strip_fences_and_prose(raw: &str) -> String {
    let trimmed = raw.trim();
    // Fast path: starts with `{`.
    if trimmed.starts_with('{') {
        return trimmed.to_owned();
    }
    // Strip a leading ```json or ``` fence + trailing fence.
    let mut working = trimmed.to_owned();
    if let Some(rest) = working.strip_prefix("```json") {
        working = rest.trim_start().to_owned();
    } else if let Some(rest) = working.strip_prefix("```") {
        working = rest.trim_start().to_owned();
    }
    if let Some(end) = working.rfind("```") {
        working.truncate(end);
    }
    let trimmed = working.trim();
    // Last-ditch: find the first `{` and the last `}` and slice.
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end >= start {
            return trimmed[start..=end].to_owned();
        }
    }
    trimmed.to_owned()
}

async fn mark_failed(
    db: &Database,
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
    if let Ok(Err(e)) = finish_res {
        tracing::warn!(
            job_id = job_id.as_str(),
            error = %e,
            "marking job Failed in DB hit an error; row may be stuck in Planning",
        );
    }
    let close = close_card(
        db,
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

    #[test]
    fn parse_plan_accepts_strict_json() {
        let plan =
            parse_plan(r#"{"thesis":"t","steps":[{"query":"q1","rationale":"r1"}]}"#).unwrap();
        assert_eq!(plan.thesis, "t");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].query, "q1");
    }

    #[test]
    fn parse_plan_strips_markdown_fences() {
        let plan =
            parse_plan("```json\n{\"thesis\":\"t\",\"steps\":[{\"query\":\"q1\"}]}\n```").unwrap();
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn parse_plan_strips_leading_prose() {
        let plan =
            parse_plan("Sure! Here's the plan: {\"thesis\":\"t\",\"steps\":[{\"query\":\"q1\"}]}")
                .unwrap();
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
    fn parse_plan_filters_empty_step_queries() {
        let plan =
            parse_plan(r#"{"thesis":"t","steps":[{"query":"   "},{"query":"real"}]}"#).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].query, "real");
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
}
