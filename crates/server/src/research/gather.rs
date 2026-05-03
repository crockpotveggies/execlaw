//! Gather phase — bounded-parallel fan-out across the planner's
//! sub-queries.
//!
//! Each worker:
//!   1. Acquire a permit from the per-job `Semaphore`
//!      (`config_research.parallel_workers`, default 3).
//!   2. `WebSearchApi::search(sub_query, cap)` → top N URLs.
//!   3. For each URL: `WebFetchApi::get(url)` → text body.
//!   4. `SubagentApi::delegate({task: "extract key facts about
//!      <sub_query> from these excerpts", context: <truncated bodies>})`.
//!   5. Persist a `ResearchNote` (DB blob + workspace JSON file) and
//!      emit a `CardProgressed` event with the per-sub-query state
//!      flipped from Running → Done / Failed.
//!
//! Hard caps from `config_research`:
//!   * `max_urls_per_subquery` clamps each worker's fetch count.
//!   * `max_pages_total` is a workspace-wide atomic counter that
//!     short-circuits further fetches once exceeded — partial gather
//!     is fine, the synthesise phase reads whatever's there.
//!   * `max_total_tokens` tallied across subagent calls; when
//!     exceeded the remaining workers skip the subagent call and
//!     emit a `Failed` note with the cap-hit error.
//!
//! The phase is cooperative w.r.t. cancellation: a `CancellationToken`
//! threaded through each worker's loop short-circuits between HTTP
//! requests and the subagent call. In-flight HTTP requests finish on
//! their own; we don't abort socket reads.
//!
//! 2026-04-29.

use crate::cards::{CardEmitError, progress_card_and_broadcast};
use crate::events::EventBus;
use crate::research::workspace::{ResearchWorkspace, WorkspaceError};
use execlaw_core::Database;
use execlaw_core::cards::{CardProgressedPayload, CardState};
use execlaw_core::ids::{ConversationId, ResearchJobId};
use execlaw_core::research::{
    PlanStep, ResearchConfig, ResearchError, ResearchJobStore, ResearchNote, ResearchPlan,
    ResearchSource, SubQueryState,
};
use execlaw_core::tool::{ApiError, SubagentApi, SubagentRequest, WebFetchApi, WebSearchApi};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum GatherError {
    #[error(transparent)]
    Store(#[from] ResearchError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    CardEmit(#[from] CardEmitError),
    #[error("gather cancelled")]
    Cancelled,
}

/// Capability handles the gather phase needs. Constructed by the
/// runner from production impls (`DuckDuckGoSearchApi`,
/// `HttpWebFetchApi`, `InferenceSubagentApi`); tests substitute
/// arc-wrapped mocks. `subagent` is `Option` so the runner can run
/// the gather workflow against tests with no inference backend
/// (each worker falls back to a placeholder excerpt and continues).
#[derive(Clone)]
pub struct GatherDeps {
    pub search: Arc<dyn WebSearchApi>,
    pub fetch: Arc<dyn WebFetchApi>,
    pub subagent: Option<Arc<dyn SubagentApi>>,
}

/// Inputs to one gather-phase pass. The runner constructs this and
/// awaits `run_gather`.
pub struct GatherCtx {
    pub db: Database,
    pub job_id: ResearchJobId,
    pub conversation_id: ConversationId,
    pub card_id: String,
    pub workspace: ResearchWorkspace,
    pub plan: ResearchPlan,
    pub config: ResearchConfig,
    pub deps: GatherDeps,
    pub events: EventBus,
    pub cancel: CancellationToken,
}

/// Run gather. Returns the final notes vector (one per plan step,
/// in stable index order). Errors propagate; partial-success notes
/// are persisted on the row regardless so the synthesise phase has
/// material to work with.
pub async fn run_gather(ctx: GatherCtx) -> Result<Vec<ResearchNote>, GatherError> {
    let GatherCtx {
        db,
        job_id,
        conversation_id,
        card_id,
        workspace,
        plan,
        config,
        deps,
        events,
        cancel,
    } = ctx;

    // Per-job atomic counters — workers consult these before the
    // expensive HTTP / inference calls so a flood of cap-busting
    // sub-queries short-circuits cheaply.
    let pages_consumed = Arc::new(AtomicU32::new(0));
    let tokens_consumed = Arc::new(AtomicU32::new(0));

    // Notes vector seeded with one Pending placeholder per step so
    // the SPA's ResearchCard renders the full plan tree from the
    // first CardProgressed even before any worker has reported.
    let mut seeded = Vec::with_capacity(plan.steps.len());
    for (i, step) in plan.steps.iter().enumerate() {
        seeded.push(ResearchNote {
            index: i as u32,
            sub_query: step.query.clone(),
            state: SubQueryState::Pending,
            excerpt: String::new(),
            sources: Vec::new(),
            tokens_used: None,
            error: None,
        });
    }
    let notes_state: Arc<Mutex<Vec<ResearchNote>>> = Arc::new(Mutex::new(seeded));

    persist_notes_and_emit_card(
        &db,
        &events,
        &job_id,
        &conversation_id,
        &card_id,
        &notes_state,
        gather_progress_for(&plan, 0.34),
        Some("Gathering".into()),
        Some("Gather phase started.".into()),
    )
    .await?;

    let semaphore = Arc::new(Semaphore::new(config.parallel_workers.max(1) as usize));
    let mut handles = Vec::with_capacity(plan.steps.len());

    for (idx, step) in plan.steps.iter().enumerate() {
        let permit_sem = semaphore.clone();
        let deps = deps.clone();
        let workspace = workspace.clone();
        let db = db.clone();
        let events_for_worker = events.clone();
        let conversation_id = conversation_id.clone();
        let card_id = card_id.clone();
        let job_id = job_id.clone();
        let notes_state = notes_state.clone();
        let pages_consumed = pages_consumed.clone();
        let tokens_consumed = tokens_consumed.clone();
        let cancel = cancel.clone();
        let plan_for_progress = plan.clone();
        let config_for_worker = config.clone();
        let step = step.clone();
        let idx_u32 = idx as u32;
        handles.push(tokio::spawn(async move {
            // Acquire a permit (semaphore is forever — never closed).
            let _permit = permit_sem
                .acquire_owned()
                .await
                .expect("semaphore not closed");

            // Cooperative cancel check before doing any work — short-
            // circuit straight into the Failed branch so the persist
            // step still runs and notes_state reflects the cancel
            // for this worker.
            let final_note = if cancel.is_cancelled() {
                failed_note(idx_u32, &step.query, "cancelled")
            } else {
                let outcome = gather_one(
                    &deps,
                    &config_for_worker,
                    &step,
                    idx_u32,
                    &pages_consumed,
                    &tokens_consumed,
                    &cancel,
                )
                .await;
                match outcome {
                    Ok(note) => note,
                    Err(e) => failed_note(idx_u32, &step.query, &e.to_string()),
                }
            };
            // Best-effort workspace write so operators can inspect
            // notes outside the SPA. DB row is the source of truth.
            if let Err(e) = workspace.write_note(&job_id, &final_note) {
                tracing::warn!(
                    job_id = job_id.as_str(),
                    index = idx_u32,
                    error = %e,
                    "writing notes/<n>.json failed; continuing — DB row is source of truth",
                );
            }
            // Splice this worker's result into the shared vector,
            // persist, and emit a card update.
            let _ = persist_one_note_and_emit_card(
                &db,
                &events_for_worker,
                &job_id,
                &conversation_id,
                &card_id,
                &notes_state,
                &final_note,
                &plan_for_progress,
            )
            .await;
            WorkerOutcome {
                index: idx_u32,
                note: final_note,
            }
        }));
    }

    let mut completed: Vec<WorkerOutcome> = Vec::with_capacity(plan.steps.len());
    for handle in handles {
        match handle.await {
            Ok(o) => completed.push(o),
            Err(join_err) => {
                tracing::warn!(error = %join_err, "gather worker panicked; treating as Failed");
                // We don't know which sub-query panicked from a
                // bare JoinError, but the worker would have updated
                // notes_state before exiting in the normal path.
                // Skip; the seeded notes vector still holds the
                // pre-panic Pending placeholder for the missing
                // worker — the synthesise phase tolerates it.
            }
        }
    }
    completed.sort_by_key(|o| o.index);
    let final_notes = notes_state.lock().await.clone();
    Ok(final_notes)
}

struct WorkerOutcome {
    index: u32,
    #[allow(dead_code)] // surfaced through the shared notes_state mutex
    note: ResearchNote,
}

async fn gather_one(
    deps: &GatherDeps,
    config: &ResearchConfig,
    step: &PlanStep,
    index: u32,
    pages_consumed: &AtomicU32,
    tokens_consumed: &AtomicU32,
    cancel: &CancellationToken,
) -> Result<ResearchNote, GatherError> {
    if cancel.is_cancelled() {
        return Err(GatherError::Cancelled);
    }

    // Search.
    let max_results = config.max_urls_per_subquery;
    let results = match deps.search.search(&step.query, max_results).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(failed_note(
                index,
                &step.query,
                &format!("search failed: {}", api_err_msg(&e)),
            ));
        }
    };

    let mut sources: Vec<ResearchSource> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    for hit in results.into_iter().take(max_results as usize) {
        if cancel.is_cancelled() {
            return Err(GatherError::Cancelled);
        }
        // Workspace-wide page cap.
        let consumed = pages_consumed.fetch_add(1, Ordering::Relaxed);
        if consumed >= config.max_pages_total {
            // Roll back the speculative increment so other workers'
            // counters stay accurate.
            pages_consumed.fetch_sub(1, Ordering::Relaxed);
            sources.push(ResearchSource {
                url: hit.url.clone(),
                title: Some(hit.title.clone()),
                fetched_ok: false,
                error: Some("max_pages_total cap reached".into()),
            });
            break;
        }
        match deps.fetch.get(&hit.url).await {
            Ok(resp) => {
                bodies.push(truncate_body(&resp.body, BODY_TRUNCATE_PER_URL));
                sources.push(ResearchSource {
                    url: resp.final_url,
                    title: Some(hit.title.clone()),
                    fetched_ok: true,
                    error: None,
                });
            }
            Err(e) => {
                pages_consumed.fetch_sub(1, Ordering::Relaxed);
                sources.push(ResearchSource {
                    url: hit.url.clone(),
                    title: Some(hit.title.clone()),
                    fetched_ok: false,
                    error: Some(api_err_msg(&e)),
                });
            }
        }
    }

    if cancel.is_cancelled() {
        return Err(GatherError::Cancelled);
    }

    // Skip the subagent call if no body fetched OR the token cap
    // already burned out.
    //
    // 2026-05-03 (rev 4): both no-search-results AND
    // every-fetch-failed are marked Failed (state). The previous
    // code returned `Done` for the every-fetch-failed case, which
    // silently let an empty-excerpt note pass synthesise's `Done`
    // filter. Synthesise then composed a report from zero usable
    // text — operator-visible result was a confident-sounding
    // empty PDF or, when EVERY worker hit this path, the misleading
    // "no notes — gather produced zero usable rows" error since
    // synthesise still saw zero notes worth using. Failing-loud
    // surfaces the real cause (search ran but every URL bounced)
    // on the card, which is what the operator needs to debug it
    // (UA blocking, rate limit, dead URLs, etc.).
    let already_burned = tokens_consumed.load(Ordering::Relaxed);
    if bodies.is_empty() {
        let no_sources = sources.is_empty();
        // Surface the per-source error reasons so the operator
        // sees WHY (HTTP 403, content-type rejected, network
        // timeout, etc.) — currently the most useful triage signal.
        let fetch_summary = if no_sources {
            "no search results".to_owned()
        } else {
            let reasons: Vec<&str> = sources
                .iter()
                .filter_map(|s| s.error.as_deref())
                .take(3)
                .collect();
            if reasons.is_empty() {
                "every fetch failed".to_owned()
            } else {
                format!("every fetch failed: {}", reasons.join("; "))
            }
        };
        return Ok(ResearchNote {
            index,
            sub_query: step.query.clone(),
            state: SubQueryState::Failed,
            excerpt: String::new(),
            sources,
            tokens_used: None,
            error: Some(fetch_summary),
        });
    }
    if already_burned >= config.max_total_tokens {
        return Ok(ResearchNote {
            index,
            sub_query: step.query.clone(),
            state: SubQueryState::Failed,
            excerpt: String::new(),
            sources,
            tokens_used: None,
            error: Some("max_total_tokens cap reached before subagent call".into()),
        });
    }

    let context_blob = bodies.join("\n\n---\n\n");
    let task = format!(
        "Extract the key facts relevant to this sub-question, in 3-6 bullets:\n{}",
        step.query
    );
    let subagent = match deps.subagent.as_ref() {
        Some(s) => s,
        None => {
            // No inference backend wired — degrade gracefully so
            // tests + dev environments without a model still see the
            // gather pipeline run.
            return Ok(ResearchNote {
                index,
                sub_query: step.query.clone(),
                state: SubQueryState::Done,
                excerpt: format!(
                    "[no subagent — {} sources fetched, no extraction performed]",
                    sources.len()
                ),
                sources,
                tokens_used: None,
                error: None,
            });
        }
    };
    let req = SubagentRequest {
        task,
        context: Some(context_blob),
        max_tokens: Some(SUBAGENT_MAX_TOKENS),
    };
    match subagent.delegate(&req).await {
        Ok(resp) => {
            if let Some(t) = resp.tokens_used {
                tokens_consumed.fetch_add(t, Ordering::Relaxed);
            }
            Ok(ResearchNote {
                index,
                sub_query: step.query.clone(),
                state: SubQueryState::Done,
                excerpt: resp.text,
                sources,
                tokens_used: resp.tokens_used,
                error: None,
            })
        }
        Err(e) => Ok(failed_note(
            index,
            &step.query,
            &format!("subagent failed: {}", api_err_msg(&e)),
        )),
    }
}

const BODY_TRUNCATE_PER_URL: usize = 4_000;
const SUBAGENT_MAX_TOKENS: u32 = 512;

fn truncate_body(body: &str, max: usize) -> String {
    if body.chars().count() <= max {
        return body.to_owned();
    }
    let mut buf: String = body.chars().take(max - 1).collect();
    buf.push('…');
    buf
}

fn failed_note(index: u32, query: &str, msg: &str) -> ResearchNote {
    ResearchNote {
        index,
        sub_query: query.to_owned(),
        state: SubQueryState::Failed,
        excerpt: String::new(),
        sources: Vec::new(),
        tokens_used: None,
        error: Some(msg.to_owned()),
    }
}

fn api_err_msg(e: &ApiError) -> String {
    e.to_string()
}

/// Compute a 0..1 progress hint for the card. Plan-only is 0.34;
/// gather-in-progress crawls toward 0.85; synthesise picks up from
/// there in C5. We don't try to be precise — the renderer also
/// surfaces per-sub-query state which is the more useful signal.
fn gather_progress_for(plan: &ResearchPlan, base: f32) -> f32 {
    let _ = plan;
    base
}

// 8 args is past clippy's threshold but every arg is heterogeneous
// (db handle, bus, ids, mutex, plan ref) and the helper is called
// from one site only. Bundling into a struct just to silence the
// lint adds churn without clarity; allow it.
#[allow(clippy::too_many_arguments)]
async fn persist_one_note_and_emit_card(
    db: &Database,
    events: &EventBus,
    job_id: &ResearchJobId,
    conversation_id: &ConversationId,
    card_id: &str,
    notes_state: &Mutex<Vec<ResearchNote>>,
    note: &ResearchNote,
    plan: &ResearchPlan,
) -> Result<(), GatherError> {
    let mut guard = notes_state.lock().await;
    let idx = note.index as usize;
    if idx < guard.len() {
        guard[idx] = note.clone();
    }
    let snapshot = guard.clone();
    drop(guard);
    let now = chrono::Utc::now().timestamp();
    let snapshot_for_db = snapshot.clone();
    let db_for_task = db.clone();
    let job_for_task = job_id.clone();
    tokio::task::spawn_blocking(move || {
        ResearchJobStore::new(&db_for_task).set_notes(&job_for_task, &snapshot_for_db, now)
    })
    .await
    .map_err(|e| GatherError::Store(ResearchError::Encoding(format!("join: {e}"))))??;
    let pct_done = snapshot
        .iter()
        .filter(|n| matches!(n.state, SubQueryState::Done | SubQueryState::Failed))
        .count() as f32
        / plan.steps.len().max(1) as f32;
    let progress = 0.34 + 0.51 * pct_done; // 0.34 (planned) → 0.85 (gather complete)
    progress_card_and_broadcast(
        db,
        events,
        conversation_id,
        "system",
        &CardProgressedPayload {
            card_id: card_id.to_owned(),
            state: Some(CardState::Running),
            progress: Some(progress.clamp(0.0, 0.85)),
            phase: Some("Gathering".into()),
            details: Some(serde_json::json!({
                "job_id": job_id.as_str(),
                "phase": "Gathering",
                "plan": plan,
                "notes": snapshot,
            })),
            actions: None,
            summary: Some(summary_for_progress(&snapshot, plan.steps.len())),
        },
    )?;
    Ok(())
}

// Same allow as persist_one_note — single call site, heterogeneous
// args that don't cluster well.
#[allow(clippy::too_many_arguments)]
async fn persist_notes_and_emit_card(
    db: &Database,
    events: &EventBus,
    job_id: &ResearchJobId,
    conversation_id: &ConversationId,
    card_id: &str,
    notes_state: &Mutex<Vec<ResearchNote>>,
    progress: f32,
    phase: Option<String>,
    summary: Option<String>,
) -> Result<(), GatherError> {
    let snapshot = notes_state.lock().await.clone();
    let now = chrono::Utc::now().timestamp();
    let snapshot_for_db = snapshot.clone();
    let db_for_task = db.clone();
    let job_for_task = job_id.clone();
    tokio::task::spawn_blocking(move || {
        ResearchJobStore::new(&db_for_task).set_notes(&job_for_task, &snapshot_for_db, now)
    })
    .await
    .map_err(|e| GatherError::Store(ResearchError::Encoding(format!("join: {e}"))))??;
    progress_card_and_broadcast(
        db,
        events,
        conversation_id,
        "system",
        &CardProgressedPayload {
            card_id: card_id.to_owned(),
            state: Some(CardState::Running),
            progress: Some(progress),
            phase,
            details: Some(serde_json::json!({
                "job_id": job_id.as_str(),
                "notes": snapshot,
            })),
            actions: None,
            summary,
        },
    )?;
    Ok(())
}

fn summary_for_progress(notes: &[ResearchNote], total: usize) -> String {
    let done = notes
        .iter()
        .filter(|n| matches!(n.state, SubQueryState::Done))
        .count();
    let failed = notes
        .iter()
        .filter(|n| matches!(n.state, SubQueryState::Failed))
        .count();
    if failed > 0 {
        format!("Gathering · {done}/{total} done · {failed} failed")
    } else {
        format!("Gathering · {done}/{total} done")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use execlaw_core::cards::{CardKind, CardOpenedPayload};
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::EventSeq;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::research::PlanStep;
    use execlaw_core::tool::{SearchResult, SubagentResponse, WebFetchResponse};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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

    struct StubSearch {
        per_query_results: usize,
    }
    #[async_trait]
    impl WebSearchApi for StubSearch {
        async fn search(
            &self,
            query: &str,
            _max_results: u32,
        ) -> Result<Vec<SearchResult>, ApiError> {
            Ok((0..self.per_query_results)
                .map(|i| SearchResult {
                    title: format!("{query} #{i}"),
                    url: format!("https://example.com/{query}/{i}"),
                    snippet: Some("snippet".into()),
                })
                .collect())
        }
        fn provider_id(&self) -> &str {
            "stub"
        }
    }

    /// Records every fetch URL so tests can assert the cap was honoured.
    struct StubFetch {
        fetched: Arc<Mutex<Vec<String>>>,
        in_flight_max: Arc<AtomicUsize>,
        in_flight: Arc<AtomicUsize>,
    }
    impl StubFetch {
        fn new() -> Self {
            Self {
                fetched: Arc::new(Mutex::new(Vec::new())),
                in_flight_max: Arc::new(AtomicUsize::new(0)),
                in_flight: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    #[async_trait]
    impl WebFetchApi for StubFetch {
        async fn get(&self, url: &str) -> Result<WebFetchResponse, ApiError> {
            let n = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.in_flight_max.fetch_max(n, AtomicOrdering::SeqCst);
            // Brief sleep so concurrent workers actually overlap.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            self.fetched.lock().await.push(url.to_owned());
            self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(WebFetchResponse {
                final_url: url.to_owned(),
                status: 200,
                content_type: Some("text/html".into()),
                body: format!("body for {url}"),
                truncated: false,
            })
        }
    }

    struct StubSubagent {
        tokens_per_call: u32,
    }
    #[async_trait]
    impl SubagentApi for StubSubagent {
        async fn delegate(&self, req: &SubagentRequest) -> Result<SubagentResponse, ApiError> {
            Ok(SubagentResponse {
                text: format!("extracted: {}", req.task.lines().last().unwrap_or("")),
                task_id: "stub".into(),
                tokens_used: Some(self.tokens_per_call),
            })
        }
    }

    fn fixture_plan(n: usize) -> ResearchPlan {
        ResearchPlan {
            thesis: "thesis".into(),
            steps: (0..n)
                .map(|i| PlanStep {
                    query: format!("q{i}"),
                    rationale: None,
                })
                .collect(),
        }
    }

    fn config_with(
        parallel_workers: u32,
        max_pages_total: u32,
        max_total_tokens: u32,
    ) -> ResearchConfig {
        ResearchConfig {
            parallel_workers,
            max_pages_total,
            max_total_tokens,
            max_urls_per_subquery: 3,
            ..Default::default()
        }
    }

    fn make_ctx(
        db: &Database,
        plan: ResearchPlan,
        config: ResearchConfig,
        deps: GatherDeps,
    ) -> GatherCtx {
        let cid = seed_conv(db, "conv-gather");
        let job_id = ResearchJobId::new();
        let card_id = "card-gather".to_owned();
        // Open the card so the progress emits land on a valid
        // existing card row (the projection layer doesn't strictly
        // need this but downstream tests can use project_card).
        crate::cards::open_card(
            db,
            &cid,
            "system",
            &CardOpenedPayload {
                card_id: card_id.clone(),
                kind: CardKind::Research,
                title: "test".into(),
                summary: "starting".into(),
                state: Some(CardState::Running),
                details: serde_json::json!({}),
                actions: vec![],
            },
        )
        .unwrap();
        // Insert a job row + persist the plan so set_notes can find
        // the row to update.
        let store = ResearchJobStore::new(db);
        store
            .insert_pending(&job_id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending(&card_id, 110).unwrap();
        store.set_planned(&job_id, &plan, 120).unwrap();
        store.mark_gathering(&job_id, 130).unwrap();
        let tmp = tempfile::tempdir().unwrap().keep();
        let workspace = ResearchWorkspace::new(tmp);
        GatherCtx {
            db: db.clone(),
            job_id,
            conversation_id: cid,
            card_id,
            workspace,
            plan,
            config,
            deps,
            events: EventBus::new(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn run_gather_happy_path_produces_one_done_note_per_step() {
        let db = fresh_db();
        let plan = fixture_plan(3);
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 2,
            }),
            fetch: Arc::new(StubFetch::new()),
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 100,
            })),
        };
        let ctx = make_ctx(&db, plan, config_with(3, 60, 100_000), deps);
        let notes = run_gather(ctx).await.unwrap();
        assert_eq!(notes.len(), 3);
        for note in &notes {
            assert_eq!(note.state, SubQueryState::Done);
            assert!(!note.excerpt.is_empty());
            assert_eq!(note.sources.len(), 2);
            assert_eq!(note.tokens_used, Some(100));
        }
    }

    #[tokio::test]
    async fn run_gather_respects_parallel_workers_semaphore() {
        let db = fresh_db();
        let plan = fixture_plan(8);
        let stub_fetch = StubFetch::new();
        let in_flight_max = stub_fetch.in_flight_max.clone();
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 2,
            }),
            fetch: Arc::new(stub_fetch),
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 50,
            })),
        };
        // Cap parallelism at 2.
        let ctx = make_ctx(&db, plan, config_with(2, 100, 100_000), deps);
        let _notes = run_gather(ctx).await.unwrap();
        // The fetch stub sleeps for 10ms; with 8 queries × 2 fetches
        // each on parallelism=2, the in-flight max should never
        // exceed 2.
        let max = in_flight_max.load(AtomicOrdering::SeqCst);
        assert!(max <= 2, "in_flight_max {max} > parallel_workers cap of 2",);
    }

    #[tokio::test]
    async fn run_gather_enforces_max_pages_total_cap() {
        let db = fresh_db();
        // 4 sub-queries × 3 fetches each = 12 attempts; cap at 5
        // means later workers run out of pages.
        let plan = fixture_plan(4);
        let stub_fetch = StubFetch::new();
        let fetched = stub_fetch.fetched.clone();
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 3,
            }),
            fetch: Arc::new(stub_fetch),
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 50,
            })),
        };
        let ctx = make_ctx(&db, plan, config_with(1, 5, 100_000), deps);
        let _notes = run_gather(ctx).await.unwrap();
        let count = fetched.lock().await.len();
        assert!(
            count <= 5,
            "fetched {count} pages, exceeded max_pages_total cap of 5",
        );
    }

    #[tokio::test]
    async fn run_gather_skips_subagent_when_token_cap_exceeded() {
        let db = fresh_db();
        let plan = fixture_plan(3);
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 1,
            }),
            fetch: Arc::new(StubFetch::new()),
            // Each call burns 1000 tokens; cap at 1500 means the
            // first call passes, subsequent workers see the cap
            // exhausted and emit Failed notes.
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 1000,
            })),
        };
        let ctx = make_ctx(&db, plan, config_with(1, 60, 1500), deps);
        let notes = run_gather(ctx).await.unwrap();
        assert_eq!(notes.len(), 3);
        let cap_failures = notes
            .iter()
            .filter(|n| {
                matches!(n.state, SubQueryState::Failed)
                    && n.error
                        .as_deref()
                        .map(|e| e.contains("max_total_tokens"))
                        .unwrap_or(false)
            })
            .count();
        assert!(
            cap_failures >= 1,
            "expected at least one Failed note citing max_total_tokens; got: {notes:#?}",
        );
    }

    #[tokio::test]
    async fn run_gather_cancellation_short_circuits_remaining_workers() {
        let db = fresh_db();
        let plan = fixture_plan(6);
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 1,
            }),
            fetch: Arc::new(StubFetch::new()),
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 50,
            })),
        };
        let mut ctx = make_ctx(&db, plan, config_with(1, 60, 100_000), deps);
        let cancel = ctx.cancel.clone();
        // Pre-cancel before run starts; every worker should hit the
        // cancellation check on entry and emit Failed.
        cancel.cancel();
        ctx.cancel = cancel;
        let notes = run_gather(ctx).await.unwrap();
        assert!(
            notes
                .iter()
                .all(|n| matches!(n.state, SubQueryState::Failed))
        );
    }

    #[tokio::test]
    async fn run_gather_falls_back_when_subagent_unavailable() {
        let db = fresh_db();
        let plan = fixture_plan(2);
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 1,
            }),
            fetch: Arc::new(StubFetch::new()),
            subagent: None,
        };
        let ctx = make_ctx(&db, plan, config_with(2, 60, 100_000), deps);
        let notes = run_gather(ctx).await.unwrap();
        assert_eq!(notes.len(), 2);
        for note in &notes {
            assert_eq!(note.state, SubQueryState::Done);
            assert!(note.excerpt.contains("no subagent"));
        }
    }

    /// 2026-05-03 (rev 4) regression: the every-fetch-failed branch
    /// used to return state=Done with an empty excerpt. Synthesise's
    /// `Done`-only filter then included the empty note, producing
    /// either a confidently-empty report OR (when EVERY worker hit
    /// the same path and synthesise ended up with zero usable rows)
    /// the misleading "no notes — gather produced zero usable rows"
    /// error. The state must be Failed and the error string must
    /// surface the per-source reasons so the operator can debug.
    #[tokio::test]
    async fn run_gather_failed_state_when_every_fetch_errors() {
        struct AlwaysFailFetch;
        #[async_trait]
        impl WebFetchApi for AlwaysFailFetch {
            async fn get(&self, url: &str) -> Result<WebFetchResponse, ApiError> {
                Err(ApiError::Storage(format!("HTTP 403 from {url}")))
            }
        }
        let db = fresh_db();
        let plan = fixture_plan(2);
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 2,
            }),
            fetch: Arc::new(AlwaysFailFetch),
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 50,
            })),
        };
        let ctx = make_ctx(&db, plan, config_with(2, 60, 100_000), deps);
        let notes = run_gather(ctx).await.unwrap();
        for note in &notes {
            assert_eq!(
                note.state,
                SubQueryState::Failed,
                "every-fetch-failed must produce Failed (not silent Done) so synthesise rejects it loudly: {note:?}",
            );
            let err = note.error.as_deref().unwrap_or("");
            assert!(
                err.starts_with("every fetch failed"),
                "error must surface the cause; got {err:?}",
            );
            // The diagnostic should include at least one per-source reason.
            assert!(
                err.contains("HTTP 403"),
                "error must surface the per-source HTTP status; got {err:?}",
            );
        }
    }

    #[tokio::test]
    async fn run_gather_when_search_returns_empty_marks_failed() {
        let db = fresh_db();
        let plan = fixture_plan(2);
        let deps = GatherDeps {
            search: Arc::new(StubSearch {
                per_query_results: 0,
            }),
            fetch: Arc::new(StubFetch::new()),
            subagent: Some(Arc::new(StubSubagent {
                tokens_per_call: 50,
            })),
        };
        let ctx = make_ctx(&db, plan, config_with(2, 60, 100_000), deps);
        let notes = run_gather(ctx).await.unwrap();
        for note in &notes {
            assert_eq!(note.state, SubQueryState::Failed);
            assert!(
                note.error
                    .as_deref()
                    .unwrap_or("")
                    .contains("no search results")
            );
        }
    }
}
