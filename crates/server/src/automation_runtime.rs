//! Automation runtime Ã¢â‚¬â€ matcher + executor (M2 of Automations).
//!
//! Plugs into the M1 bus by providing an [`EventHandler`] that
//! `cmd_serve` installs in place of [`noop_handler`]. For each
//! delivered event, the handler:
//!
//!   1. Looks up enabled automations whose `trigger.kind` matches the
//!      event's kind ([`AutomationStore::list_enabled_for_kind`] Ã¢â‚¬â€
//!      indexed lookup).
//!   2. Evaluates each automation's optional `trigger.when` Rhai
//!      predicate against the event context. Predicate `false` Ã¢â€ â€™
//!      skip; `true` (or absent) Ã¢â€ â€™ schedule a run.
//!   3. Mints a pending [`AutomationRunRow`] and walks the typed
//!      graph node-by-node, checkpointing each step via
//!      [`AutomationRunStore::append_trace`] before advancing.
//!
//! Edge routing: edges have an optional `when` Rhai bool. The
//! executor picks the first edge from the current node whose `when`
//! evaluates truthy (or has no `when`). No matching edge = implicit
//! end. The `END_SENTINEL` and explicit [`NodeKind::Terminal`] both
//! mean the run is over.
//!
//! Expression evaluation uses a freshly-constructed [`rhai::Engine`]
//! per call with tight sandbox limits Ã¢â‚¬â€ no host capabilities, no I/O.
//! Filter/Transform/Branch are the only kinds that exercise it in M2.
//!
//! Concurrency: the handler runs on the bus's worker pool. Each
//! automation run is fully synchronous internally (SQLite + Rhai),
//! so we shuttle the work onto a `spawn_blocking` thread to avoid
//! parking the tokio runtime on SQLite writes.

use crate::automation_agent::{AskAgentRequest, AutomationsAgentPool};
use crate::automation_bus::EventHandler;
use execlaw_core::Database;
use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use execlaw_core::automation_bus::BusEventRow;
use execlaw_core::automation_runs::{AutomationRunStatus, AutomationRunStore, StepTrace};
use execlaw_core::automations::{
    AutomationDef, AutomationRow, AutomationStore, END_SENTINEL, NodeDef, NodeKind,
    TRIGGER_SENTINEL, TriggerDef, parse_ask_agent_config,
};
use execlaw_core::ids::AlertId;
use execlaw_plugin_host::PluginHost;
use rhai::{Dynamic, Engine, Scope};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

/// Per-run handles for the side-effect executors. Threaded into
/// [`execute_node`] so M4-and-beyond kinds (Notify, CallPlugin, â€¦)
/// can reach the relevant subsystem without us re-plumbing every
/// signature each time we land a new kind.
#[derive(Clone)]
pub struct ExecutorContext {
    pub db: Database,
    pub pool: AutomationsAgentPool,
    /// Optional so tests that don't exercise CallPlugin can wire the
    /// runtime without spinning up a full plugin host. A `None` here
    /// turns CallPlugin into a clean per-node error rather than a
    /// runtime panic.
    pub plugin_host: Option<PluginHost>,
    /// M6 — live event hub. Executor publishes `FlowChannelEvent`
    /// frames during graph execution; the SPA subscribes by `run_id`.
    pub flow_channel: crate::flow_channel::FlowChannelHub,
    /// M6 — UiEvent broadcast bus, forwarded into the ReplyRouter
    /// so the `chat_append` handler can publish
    /// `UiEvent::ChatMessageOutbound` to live SPA subscribers.
    pub events: Option<crate::events::EventBus>,
    /// M6 — event-log HMAC key, forwarded into the ReplyRouter so
    /// chat replies persist into `state_events` with the proper
    /// HMAC chain.
    pub event_log_hmac_key: Option<std::sync::Arc<Vec<u8>>>,
}

impl ExecutorContext {
    pub fn new(db: Database, pool: AutomationsAgentPool, plugin_host: Option<PluginHost>) -> Self {
        Self {
            db,
            pool,
            plugin_host,
            flow_channel: crate::flow_channel::FlowChannelHub::new(),
            events: None,
            event_log_hmac_key: None,
        }
    }

    pub fn with_flow_channel(mut self, hub: crate::flow_channel::FlowChannelHub) -> Self {
        self.flow_channel = hub;
        self
    }

    pub fn with_events(mut self, events: crate::events::EventBus) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_event_log_hmac_key(
        mut self,
        key: Option<std::sync::Arc<Vec<u8>>>,
    ) -> Self {
        self.event_log_hmac_key = key;
        self
    }
}

/// Construct an [`EventHandler`] that drives the automation matcher
/// + executor against `db`. Drop this into `AutomationBus::spawn` in
/// place of `noop_handler`.
///
/// The `agent_pool` is the seam that lets cmd_serve plug in the real
/// LLM-backed `InferenceAgentInvoker` while tests use a
/// `StubAgentInvoker`. Without an `AskAgent` node in a flow, the pool
/// is never invoked Ã¢â‚¬â€ but it must always be present so the runtime
/// has a well-defined behavior for AskAgent regardless of LLM
/// availability.
pub fn build_handler(ctx: ExecutorContext) -> EventHandler {
    Arc::new(move |row: BusEventRow| {
        let ctx = ctx.clone();
        Box::pin(async move {
            // SQLite + Rhai are sync; the agent pool's invocation is
            // async but we bridge across `block_on` inside the
            // spawn_blocking thread (cheap Ã¢â‚¬â€ the only awaiting work
            // is the semaphore acquire + the model HTTP round-trip,
            // both of which we want to serialize per-run anyway).
            if let Err(e) = tokio::task::spawn_blocking(move || {
                run_matching_automations(&ctx, &row);
            })
            .await
            {
                warn!(error = %e, "automation runtime: spawn_blocking failed");
            }
        })
    })
}

/// Top-level matcher: list enabled automations for the event's kind,
/// filter by trigger predicate, run each that matches.
fn run_matching_automations(ctx: &ExecutorContext, evt: &BusEventRow) {
    let store = AutomationStore::new(&ctx.db);
    let matched = match store.list_enabled_for_kind(&evt.kind) {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, kind = %evt.kind, "automation runtime: list_enabled_for_kind failed");
            return;
        }
    };
    if matched.is_empty() {
        debug!(
            event_id = %evt.id,
            kind = %evt.kind.as_str(),
            "automation runtime: no automations match event kind",
        );
        return;
    }
    let event_ctx = event_context(evt);
    for automation in matched {
        if !trigger_matches(&automation.definition.trigger, &event_ctx) {
            debug!(
                event_id = %evt.id,
                automation_id = %automation.id,
                "automation runtime: trigger predicate rejected event",
            );
            continue;
        }
        run_one(ctx, &automation, evt, &event_ctx);
    }
}

/// Build the Rhai-facing event-shape map. We mirror the
/// [`BusEvent`] envelope so flow authors write `event.payload.foo`
/// without having to think about persistence shape.
fn event_context(evt: &BusEventRow) -> serde_json::Value {
    // The Rhai scope sees one object: `event`. Anything not exposed here
    // is invisible to trigger.when / edge.when expressions, which is
    // why the envelope is included verbatim — flows need to discriminate
    // between operator-internal events and external traffic, and need
    // sender identity for routing decisions. See audit fix #2.
    serde_json::json!({
        "id": evt.id,
        "kind": evt.kind,
        "source": evt.source,
        "received_at": evt.received_at,
        "payload": evt.payload,
        "envelope": evt.envelope,
        "internal": evt.internal,
    })
}

fn trigger_matches(trigger: &TriggerDef, event_ctx: &serde_json::Value) -> bool {
    let Some(expr) = trigger.when.as_ref() else {
        return true;
    };
    let mut scope = build_scope_with_event(event_ctx);
    match eval_bool(expr, &mut scope) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                error = %e,
                expr = %expr,
                "automation runtime: trigger.when rhai eval failed; treating as no-match",
            );
            false
        }
    }
}

fn run_one(
    ctx: &ExecutorContext,
    automation: &AutomationRow,
    evt: &BusEventRow,
    event_ctx: &serde_json::Value,
) {
    let run_store = AutomationRunStore::new(&ctx.db);
    let started_at = chrono::Utc::now().timestamp_millis();
    let run_id = match run_store.insert_pending(&automation.id, &evt.id, started_at) {
        Ok(id) => id,
        Err(e) => {
            warn!(
                error = %e,
                automation_id = %automation.id,
                event_id = %evt.id,
                "automation runtime: failed to insert pending run row",
            );
            return;
        }
    };

    // Accumulated state: each completed node's output keyed by id,
    // plus `event` for the trigger payload.
    let mut state: HashMap<String, serde_json::Value> = HashMap::new();
    state.insert("event".to_string(), event_ctx.clone());

    // Inline trace sink: each node-boundary advance calls
    // `run_store.append_trace(run_id, &trace)`. The closure indirection
    // is what lets the same executor body power both live runs (DB
    // checkpointing) and dry runs (in-memory Vec sink) without
    // duplication.
    let mut trace_sink = |trace: StepTrace| {
        if let Err(e) = run_store.append_trace(&run_id, &trace) {
            warn!(
                error = %e,
                run_id = %run_id,
                node_id = %trace.node_id,
                "automation runtime: failed to append step trace",
            );
        }
    };
    let outcome = execute_graph(
        &automation.definition,
        &mut state,
        ctx,
        &evt.envelope,
        &run_id,
        &mut trace_sink,
    );
    let finished_at = chrono::Utc::now().timestamp_millis();
    let final_status = match outcome {
        ExecOutcome::Success => AutomationRunStatus::Success,
        ExecOutcome::Skipped => AutomationRunStatus::Skipped,
        ExecOutcome::Failed => AutomationRunStatus::Failed,
    };
    if let Err(e) = run_store.finish(&run_id, final_status, finished_at) {
        warn!(
            error = %e,
            run_id = %run_id,
            "automation runtime: failed to finalize run row",
        );
    }
    // M6 — emit the run-finished event onto the live channel.
    ctx.flow_channel.publish(crate::flow_channel::FlowChannelEvent::RunFinished {
        run_id: run_id.to_string(),
        outcome: match outcome {
            ExecOutcome::Success => "success",
            ExecOutcome::Skipped => "skipped",
            ExecOutcome::Failed => "failed",
        }
        .to_owned(),
    });
}

/// Result of a [`dry_run`] Ã¢â‚¬â€ outcome + captured per-node trace.
/// The HTTP `POST /test-run` endpoint serializes this directly.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct DryRunResult {
    pub outcome: ExecOutcome,
    pub step_traces: Vec<StepTrace>,
}

/// Run the executor against an automation + a synthetic / captured
/// event, capturing per-node traces in memory. Does NOT persist a
/// run row Ã¢â‚¬â€ used by the editor's "Test run" button so the operator
/// can iterate on a definition without polluting `state_automation_runs`.
///
/// The agent pool is reused, so an `AskAgent` node in a test run will
/// actually invoke the LLM through the same path as live dispatch.
/// Callers that don't want to hit the model should swap the pool's
/// invoker (the M3 test fixtures show how Ã¢â‚¬â€ `StubAgentInvoker`).
pub fn dry_run(
    ctx: &ExecutorContext,
    automation: &AutomationRow,
    sample: &BusEventRow,
) -> DryRunResult {
    dry_run_with_id(ctx, automation, sample, None)
}

/// Like [`dry_run`], but lets the caller supply the run id. When
/// `client_run_id` is `Some`, the FlowChannelHub publishes events
/// under that id so SPA SSE subscribers can correlate (audit fix
/// #8). When `None`, falls back to the legacy `dry-{uuid}` mint.
pub fn dry_run_with_id(
    ctx: &ExecutorContext,
    automation: &AutomationRow,
    sample: &BusEventRow,
    client_run_id: Option<String>,
) -> DryRunResult {
    let event_ctx = event_context(sample);
    let mut state: HashMap<String, serde_json::Value> = HashMap::new();
    state.insert("event".to_string(), event_ctx);
    let mut traces: Vec<StepTrace> = Vec::new();
    let dry_run_id =
        client_run_id.unwrap_or_else(|| format!("dry-{}", uuid::Uuid::new_v4()));
    let outcome = execute_graph(
        &automation.definition,
        &mut state,
        ctx,
        &sample.envelope,
        &dry_run_id,
        &mut |t: StepTrace| traces.push(t),
    );
    DryRunResult {
        outcome,
        step_traces: traces,
    }
}

/// Public terminal state for one walk through the graph. Live runs
/// translate this into [`AutomationRunStatus`]; dry runs return it
/// to the caller verbatim alongside the captured traces.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
    Success,
    Skipped,
    Failed,
}

/// Walk the graph from `TRIGGER_SENTINEL` until we hit an end
/// condition. State is mutated in place as nodes write outputs.
///
/// The trace sink is invoked at each node boundary BEFORE the run
/// advances. Live runs pass a closure that checkpoints the trace into
/// SQLite via `AutomationRunStore::append_trace`; dry runs pass a
/// closure that pushes into a `Vec<StepTrace>`. Same executor body
/// powers both paths.
fn execute_graph(
    def: &AutomationDef,
    state: &mut HashMap<String, serde_json::Value>,
    ctx: &ExecutorContext,
    envelope: &execlaw_core::event_envelope::EventEnvelope,
    run_id: &str,
    trace_sink: &mut dyn FnMut(StepTrace),
) -> ExecOutcome {
    let mut current = TRIGGER_SENTINEL.to_string();
    // Defense in depth Ã¢â‚¬â€ refuse to walk pathologically long graphs.
    // The validator should already reject cycles in M3+, but for
    // now we cap at 256 hops.
    let max_hops = 256;
    for _ in 0..max_hops {
        let next = match pick_next_edge(def, &current, state) {
            Ok(Some(to)) => to,
            Ok(None) => return ExecOutcome::Success, // implicit end
            Err(msg) => {
                let trace = StepTrace {
                    node_id: format!("edge-from:{current}"),
                    input: serde_json::Value::Null,
                    output: serde_json::Value::Null,
                    ms: 0,
                    error: Some(msg),
                };
                trace_sink(trace);
                return ExecOutcome::Failed;
            }
        };
        if next == END_SENTINEL {
            return ExecOutcome::Success;
        }
        let node = def
            .nodes
            .iter()
            .find(|n| n.id == next)
            .expect("validator guarantees edge targets resolve");

        let start = Instant::now();
        let input_snapshot = snapshot_state(state);
        // M6 — emit NodeStarted onto the run channel.
        ctx.flow_channel.publish(crate::flow_channel::FlowChannelEvent::NodeStarted {
            run_id: run_id.to_string(),
            node_id: node.id.clone(),
            node_kind: node.kind.as_str().to_owned(),
        });
        let exec_result = execute_node(node, state, ctx, envelope);
        let ms = start.elapsed().as_millis() as u64;

        match exec_result {
            NodeOutcome::Output(value) => {
                let trace = StepTrace {
                    node_id: node.id.clone(),
                    input: input_snapshot,
                    output: value.clone(),
                    ms,
                    error: None,
                };
                trace_sink(trace);
                ctx.flow_channel
                    .publish(crate::flow_channel::FlowChannelEvent::NodeFinished {
                        run_id: run_id.to_string(),
                        node_id: node.id.clone(),
                        output: value.clone(),
                        ms,
                        error: None,
                    });
                state.insert(node.id.clone(), value);
                current = node.id.clone();
            }
            NodeOutcome::Drop => {
                let trace = StepTrace {
                    node_id: node.id.clone(),
                    input: input_snapshot,
                    output: serde_json::Value::Null,
                    ms,
                    error: None,
                };
                trace_sink(trace);
                return ExecOutcome::Skipped;
            }
            NodeOutcome::Terminal => {
                let trace = StepTrace {
                    node_id: node.id.clone(),
                    input: input_snapshot,
                    output: serde_json::Value::Null,
                    ms,
                    error: None,
                };
                trace_sink(trace);
                return ExecOutcome::Success;
            }
            NodeOutcome::Error(msg) => {
                let trace = StepTrace {
                    node_id: node.id.clone(),
                    input: input_snapshot,
                    output: serde_json::Value::Null,
                    ms,
                    error: Some(msg),
                };
                trace_sink(trace);
                return ExecOutcome::Failed;
            }
        }
    }
    // Hit the hop cap Ã¢â‚¬â€ record a failure and bail.
    let trace = StepTrace {
        node_id: "(executor)".into(),
        input: serde_json::Value::Null,
        output: serde_json::Value::Null,
        ms: 0,
        error: Some(format!(
            "automation exceeded {max_hops} hops; suspected cycle"
        )),
    };
    trace_sink(trace);
    ExecOutcome::Failed
}

/// Snapshot of state for the per-step trace. We deliberately don't
/// re-serialize the entire state if it's huge; cap at the immediate
/// context the node would see (event + most recently produced output).
/// For M2, full state is small (a handful of node outputs) so we
/// just serialize it directly.
fn snapshot_state(state: &HashMap<String, serde_json::Value>) -> serde_json::Value {
    serde_json::Value::Object(state.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Pick the first outgoing edge from `from` whose `when` is truthy
/// (or has no `when`). Returns `Ok(None)` if no edge matches.
fn pick_next_edge(
    def: &AutomationDef,
    from: &str,
    state: &HashMap<String, serde_json::Value>,
) -> Result<Option<String>, String> {
    for edge in def.edges.iter().filter(|e| e.from == from) {
        let take = match edge.when.as_ref() {
            None => true,
            Some(expr) => {
                let mut scope = build_scope_from_state(state);
                eval_bool(expr, &mut scope)
                    .map_err(|e| format!("edge.when rhai eval failed: {e}"))?
            }
        };
        if take {
            return Ok(Some(edge.to.clone()));
        }
    }
    Ok(None)
}

#[derive(Debug)]
enum NodeOutcome {
    /// Node produced a value; bind under the node's id and advance.
    Output(serde_json::Value),
    /// Filter said no Ã¢â‚¬â€ terminate the run as `Skipped`.
    Drop,
    /// Explicit Terminal node Ã¢â‚¬â€ terminate as `Success`.
    Terminal,
    /// Node failed at runtime Ã¢â‚¬â€ terminate as `Failed`.
    Error(String),
}

fn execute_node(
    node: &NodeDef,
    state: &HashMap<String, serde_json::Value>,
    ctx: &ExecutorContext,
    envelope: &execlaw_core::event_envelope::EventEnvelope,
) -> NodeOutcome {
    match node.kind {
        NodeKind::Filter => execute_filter(node, state),
        NodeKind::Transform => execute_transform(node, state),
        NodeKind::Branch => {
            // Branch is a no-op routing junction Ã¢â‚¬â€ outputs an empty
            // map so downstream `{{branch.*}}` references resolve
            // without surprises. The actual branching lives in the
            // outgoing edges' `when` clauses.
            NodeOutcome::Output(serde_json::json!({}))
        }
        NodeKind::Terminal => NodeOutcome::Terminal,
        NodeKind::AskAgent => execute_ask_agent(node, state, &ctx.pool),
        NodeKind::Notify => execute_notify(node, state, &ctx.db),
        NodeKind::CallPlugin => execute_call_plugin(node, state, ctx.plugin_host.as_ref()),
        NodeKind::SendReply => execute_send_reply(node, state, ctx, envelope),
        _ => NodeOutcome::Error(format!(
            "node kind '{}' not implemented in this milestone",
            node.kind.as_str()
        )),
    }
}

/// SendReply (M6) — build a `ReplyPayload` per the node's config and
/// hand it to the [`crate::reply_router`]. Output = serialized
/// `RouteResult` for downstream Branches.
fn execute_send_reply(
    node: &NodeDef,
    state: &HashMap<String, serde_json::Value>,
    ctx: &ExecutorContext,
    envelope: &execlaw_core::event_envelope::EventEnvelope,
) -> NodeOutcome {
    let cfg = &node.config;
    let source = cfg
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("from_agent");

    let payload = match build_reply_payload(source, cfg, state) {
        Ok(p) => p,
        Err(msg) => return NodeOutcome::Error(format!("SendReply: {msg}")),
    };

    let effective_envelope = match cfg.get("target_override") {
        Some(v) if !v.is_null() => match serde_json::from_value::<
            execlaw_core::event_envelope::OriginRef,
        >(v.clone())
        {
            Ok(origin) => execlaw_core::event_envelope::EventEnvelope {
                origin,
                identity: envelope.identity.clone(),
                correlation_id: envelope.correlation_id.clone(),
                parent_event_id: envelope.parent_event_id.clone(),
            },
            Err(e) => {
                return NodeOutcome::Error(format!(
                    "SendReply: target_override is not a valid OriginRef: {e}"
                ));
            }
        },
        _ => envelope.clone(),
    };

    let mut router_ctx =
        crate::reply_router::RouterCtx::new(ctx.db.clone(), ctx.plugin_host.clone());
    if let Some(events) = ctx.events.clone() {
        router_ctx = router_ctx.with_events(events);
    }
    router_ctx = router_ctx.with_event_log_hmac_key(ctx.event_log_hmac_key.clone());

    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            return NodeOutcome::Error(
                "SendReply: no tokio runtime in scope".into(),
            );
        }
    };
    let result = handle.block_on(crate::reply_router::route(
        &router_ctx,
        &effective_envelope,
        payload,
    ));
    NodeOutcome::Output(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null))
}

fn build_reply_payload(
    source: &str,
    cfg: &serde_json::Value,
    state: &HashMap<String, serde_json::Value>,
) -> Result<execlaw_core::reply::ReplyPayload, String> {
    use execlaw_core::reply::{ReplyHints, ReplyPart, ReplyPayload};

    let hints: ReplyHints = cfg
        .get("hints")
        .cloned()
        .map(|h| serde_json::from_value::<ReplyHints>(h).unwrap_or_default())
        .unwrap_or_default();

    match source {
        "from_agent" | "from_node_output" => {
            let node_id = cfg
                .get("from_node")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
                .or_else(|| infer_last_ask_node(state))
                .ok_or_else(|| "could not infer upstream node — set config.from_node".to_owned())?;
            let out = state
                .get(&node_id)
                .ok_or_else(|| format!("upstream node '{node_id}' has no recorded output"))?;
            // AskAgent emits `{ tool, args }`; the exit-tool args may
            // already be a `ReplyPayload` shape. If so, use it; else
            // synthesize.
            let args = out.get("args").cloned().unwrap_or(out.clone());
            if let Ok(p) = serde_json::from_value::<ReplyPayload>(args.clone()) {
                let mut p = p;
                p.hints = hints;
                Ok(p)
            } else {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                Ok(ReplyPayload {
                    text,
                    parts: vec![],
                    hints,
                })
            }
        }
        "template" => {
            let text_template = cfg.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let text = render_template(text_template, state);
            let parts: Vec<ReplyPart> = cfg
                .get("parts")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            Ok(ReplyPayload {
                text,
                parts,
                hints,
            })
        }
        other => Err(format!(
            "unknown SendReply.source '{other}' (expected from_agent | from_node_output | template)"
        )),
    }
}

fn infer_last_ask_node(state: &HashMap<String, serde_json::Value>) -> Option<String> {
    let mut candidates: Vec<&String> = state
        .iter()
        .filter(|(_, v)| v.get("tool").is_some())
        .map(|(k, _)| k)
        .collect();
    candidates.sort_by_key(|k| std::cmp::Reverse(k.len()));
    candidates.into_iter().next().cloned()
}

/// Notify (M6) Ã¢â‚¬â€ insert a row into `state_alerts` via
/// [`AlertStore::insert_firing`]. The alert's `source` defaults to
/// `automation:<node_id>` so operators can fingerprint-dedup against
/// the producing flow.
///
/// Config (validated upstream):
/// ```text
/// { "title": string,
///   "detail": optional string,
///   "severity": "Critical" | "Error" | "Warning" | "Info",
///   "source": optional string }
/// ```
///
/// Template substitution: `title` and `detail` pass through
/// [`render_template`] so the alert can reference `{{event.payload.x}}`
/// and upstream node outputs.
///
/// Output: `{ "alert_id": "<id>" }` Ã¢â‚¬â€ downstream nodes can route on
/// alert_id presence if they want to ack the notification.
fn execute_notify(
    node: &NodeDef,
    state: &HashMap<String, serde_json::Value>,
    db: &Database,
) -> NodeOutcome {
    let cfg = &node.config;
    let title_raw = match cfg.get("title").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            return NodeOutcome::Error(
                "Notify node missing or empty config.title (must be a non-empty string)".into(),
            );
        }
    };
    let detail_raw = cfg.get("detail").and_then(|v| v.as_str());
    let severity = match cfg.get("severity").and_then(|v| v.as_str()) {
        Some(s) => match Severity::parse(s) {
            Some(sev) => sev,
            None => {
                return NodeOutcome::Error(format!(
                    "Notify node: unknown severity '{s}' (expected Critical|Error|Warning|Info)"
                ));
            }
        },
        None => Severity::Warning, // Sensible default: louder than Info, less alarming than Error.
    };
    let source = cfg
        .get("source")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("automation:{}", node.id));

    let title = render_template(title_raw, state);
    let detail = detail_raw.map(|d| render_template(d, state));

    // Fingerprint = `<source>::<title>` so re-firing the same alert
    // bumps occurrence_count instead of creating dup rows. Author can
    // make the fingerprint unique-per-instance by embedding
    // `{{event.id}}` in the title.
    let fingerprint = format!("{}::{}", source, title);
    let now = chrono::Utc::now().timestamp_millis();
    let id = AlertId::new();
    let row = AlertRow {
        id: id.clone(),
        fingerprint,
        severity,
        source,
        title,
        detail,
        context_json: None,
        status: AlertStatus::Firing,
        first_seen_at: now,
        last_seen_at: now,
        occurrence_count: 1,
        resolved_at: None,
        resolved_by: None,
        ack_at: None,
        ack_by: None,
        snooze_until: None,
        incident_id: None,
        actions_json: None,
    };
    let store = AlertStore::new(db);
    match store.insert_firing(&row) {
        Ok(()) => NodeOutcome::Output(serde_json::json!({
            "alert_id": id.as_str(),
        })),
        Err(e) => NodeOutcome::Error(format!("Notify: insert_firing failed: {e}")),
    }
}

/// CallPlugin (M6) Ã¢â‚¬â€ dispatch to a plugin-registered tool by name.
/// Reuses [`PluginHost::call_tool`] which is the same path the
/// LLM-callable tool registry uses.
///
/// Config:
/// ```text
/// { "tool": string,           // registered tool name, e.g. "signal.send_message"
///   "args": object }          // passed verbatim to the tool
/// ```
///
/// Auth posture: automations run with controller privileges (admin-
/// authored, system-context), so we pass `caps=["*"]` (wildcard) and
/// `trust="Controller"`. This is consistent with the routine
/// subsystem's auth posture.
///
/// Output: the tool's raw return value Ã¢â‚¬â€ downstream nodes can pluck
/// fields out via `{{node_id.field}}`.
fn execute_call_plugin(
    node: &NodeDef,
    state: &HashMap<String, serde_json::Value>,
    plugin_host: Option<&PluginHost>,
) -> NodeOutcome {
    let cfg = &node.config;
    let tool = match cfg.get("tool").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_owned(),
        _ => {
            return NodeOutcome::Error(
                "CallPlugin node missing or empty config.tool (must be a registered tool name)"
                    .into(),
            );
        }
    };
    // Args default to an empty object so authors can omit the field
    // for parameterless tools.
    let args_raw = cfg.get("args").cloned().unwrap_or(serde_json::json!({}));
    // Template-render any string leaves in the args so the author can
    // reference `{{event.payload.x}}` without hand-stringifying first.
    let args = render_template_in_value(&args_raw, state);

    let Some(host) = plugin_host else {
        return NodeOutcome::Error(
            "CallPlugin: no plugin host wired into the runtime (tests without plugin support \
             reach this branch Ã¢â‚¬â€ production builds always have one)"
                .into(),
        );
    };

    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            return NodeOutcome::Error(
                "CallPlugin: no tokio runtime in scope (executor must run under spawn_blocking)"
                    .into(),
            );
        }
    };
    let result = handle.block_on(host.call_tool(&tool, args, &["*"], Some("Controller")));
    match result {
        Ok(v) => NodeOutcome::Output(v),
        Err(e) => NodeOutcome::Error(format!("CallPlugin '{tool}' failed: {e}")),
    }
}

/// Recursively apply [`render_template`] to every string leaf in a
/// JSON value. Used by CallPlugin so the operator can stash
/// `{{event.payload.x}}` references inside the args object's fields
/// rather than having to pre-bake them via a Transform node.
fn render_template_in_value(
    v: &serde_json::Value,
    state: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(render_template(s, state)),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|x| render_template_in_value(x, state))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                out.insert(k.clone(), render_template_in_value(val, state));
            }
            serde_json::Value::Object(out)
        }
        // Numbers, bools, null pass through unchanged.
        other => other.clone(),
    }
}

/// AskAgent (M3) Ã¢â‚¬â€ delegates to the [`AutomationsAgentPool`]. The
/// pool's semaphore bounds in-flight invocations; the invoker
/// translates the request into a single chat-completion call
/// (production) or a scripted reply (tests).
///
/// Node output shape: `{ "tool": "<exit-tool-name>", "args": {...} }`.
/// Downstream edges route on `<node_id>.tool`; downstream nodes can
/// read `<node_id>.args.*` for the structured payload the agent
/// produced.
///
/// Template substitution: `prompt` and each entry of `attachments`
/// pass through [`render_template`] before reaching the invoker, so
/// authors can write `{{event.payload.image_url}}` in the JSON
/// definition and have it resolved against the run's state map.
///
/// M3a single-turn limitation: `max_turns` Ã¢â€°Â¥ 1 is treated as 1.
/// The framework is sized for multi-turn (the invoker exposes the
/// effective turn count) but the loop body is a follow-up.
fn execute_ask_agent(
    node: &NodeDef,
    state: &HashMap<String, serde_json::Value>,
    pool: &AutomationsAgentPool,
) -> NodeOutcome {
    let mut cfg = match parse_ask_agent_config(&node.config) {
        Ok(c) => c,
        Err(msg) => return NodeOutcome::Error(msg),
    };
    // Substitute `{{path.to.field}}` references in prompt + each
    // attachment URL using the current state. Filter/Transform's
    // Rhai expressions already access state via field syntax; this
    // is the string-substitution analog for free-text fields.
    cfg.prompt = render_template(&cfg.prompt, state);
    cfg.attachments = cfg
        .attachments
        .into_iter()
        .map(|a| render_template(&a, state))
        .collect();
    let req = AskAgentRequest { config: cfg };
    // We're inside a `spawn_blocking` thread; `Handle::current()`
    // gives us the calling tokio runtime so we can drive the async
    // invocation to completion without holding a worker.
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            return NodeOutcome::Error(
                "AskAgent: no tokio runtime in scope (executor must run under spawn_blocking)"
                    .into(),
            );
        }
    };
    match handle.block_on(pool.invoke(&req)) {
        Ok(call) => NodeOutcome::Output(serde_json::json!({
            "tool": call.name,
            "args": call.args,
        })),
        Err(e) => NodeOutcome::Error(format!("{e}")),
    }
}

/// Replace `{{path.to.field}}` tokens with the corresponding value
/// from `state`. Unresolved paths are preserved verbatim (so the
/// resulting prompt makes the breakage visible during debugging
/// rather than silently dropping the reference).
///
/// Scoped to `AskAgent` for M3a. M4 may apply the same rendering to
/// Notify's `message` and HttpFetch's `url`/`body`.
pub(crate) fn render_template(s: &str, state: &HashMap<String, serde_json::Value>) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `{{` opener.
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Scan forward for matching `}}`.
            let mut j = i + 2;
            let mut found = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                let path = std::str::from_utf8(&bytes[i + 2..end]).unwrap_or("").trim();
                match lookup_path(path, state) {
                    Some(v) => {
                        out.push_str(&value_to_string(&v));
                    }
                    None => {
                        // Preserve the literal Ã¢â‚¬â€ visible breakage is
                        // better than silent dropping.
                        out.push_str(&s[i..end + 2]);
                    }
                }
                i = end + 2;
                continue;
            }
        }
        // Push the next char (UTF-8 safe via the original &str).
        let ch_start = i;
        let ch_end = (1..=4)
            .map(|k| ch_start + k)
            .find(|&end| s.is_char_boundary(end))
            .unwrap_or(s.len());
        out.push_str(&s[ch_start..ch_end]);
        i = ch_end;
    }
    out
}

/// Walk a dotted path against the state map and return the leaf
/// value. Returns `None` if any segment fails to resolve.
fn lookup_path(
    path: &str,
    state: &HashMap<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut cur = state.get(first)?.clone();
    for p in parts {
        cur = cur.get(p)?.clone();
    }
    Some(cur)
}

/// Convert a JSON value to the string a template would naturally
/// substitute in. Strings are unquoted; numbers/bools render their
/// natural lexical form; objects/arrays use their compact JSON form
/// (rare in templates but harmless when it does happen).
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn execute_filter(node: &NodeDef, state: &HashMap<String, serde_json::Value>) -> NodeOutcome {
    let Some(expr) = node.config.get("expr").and_then(|v| v.as_str()) else {
        return NodeOutcome::Error(
            "Filter node missing config.expr (must be Rhai bool expression)".into(),
        );
    };
    let mut scope = build_scope_from_state(state);
    match eval_bool(expr, &mut scope) {
        Ok(true) => NodeOutcome::Output(serde_json::json!({"passed": true})),
        Ok(false) => NodeOutcome::Drop,
        Err(e) => NodeOutcome::Error(format!("Filter expr rhai eval failed: {e}")),
    }
}

fn execute_transform(node: &NodeDef, state: &HashMap<String, serde_json::Value>) -> NodeOutcome {
    let Some(expr) = node.config.get("expr").and_then(|v| v.as_str()) else {
        return NodeOutcome::Error(
            "Transform node missing config.expr (must be Rhai expression)".into(),
        );
    };
    let mut scope = build_scope_from_state(state);
    match eval_value(expr, &mut scope) {
        Ok(v) => NodeOutcome::Output(v),
        Err(e) => NodeOutcome::Error(format!("Transform expr rhai eval failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Rhai expression evaluation. Sandbox is intentionally tight: no
// modules, no eval, no globals beyond a small set of helpers, hard
// caps on ops + depth + memory growth. Filter / Transform / Branch
// only need pure expressions.
// ---------------------------------------------------------------------------

/// Sandbox limits Ã¢â‚¬â€ same shape as the script-tier plugin engine but
/// halved (we're evaluating one-line predicates, not whole plugins).
const RHAI_MAX_OPS: u64 = 100_000;
const RHAI_MAX_CALL_DEPTH: usize = 32;
const RHAI_MAX_EXPR_DEPTH: usize = 32;
const RHAI_MAX_STRING_LEN: usize = 65_536;
const RHAI_MAX_ARRAY_LEN: usize = 10_000;
const RHAI_MAX_MAP_LEN: usize = 10_000;

fn make_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(RHAI_MAX_OPS);
    engine.set_max_call_levels(RHAI_MAX_CALL_DEPTH);
    engine.set_max_expr_depths(RHAI_MAX_EXPR_DEPTH, RHAI_MAX_EXPR_DEPTH);
    engine.set_max_string_size(RHAI_MAX_STRING_LEN);
    engine.set_max_array_size(RHAI_MAX_ARRAY_LEN);
    engine.set_max_map_size(RHAI_MAX_MAP_LEN);
    engine
}

fn build_scope_with_event(event_ctx: &serde_json::Value) -> Scope<'static> {
    let mut scope = Scope::new();
    scope.push_dynamic("event", json_to_dynamic(event_ctx));
    scope
}

fn build_scope_from_state(state: &HashMap<String, serde_json::Value>) -> Scope<'static> {
    let mut scope = Scope::new();
    for (k, v) in state {
        scope.push_dynamic(k.clone(), json_to_dynamic(v));
    }
    scope
}

/// Convert a `serde_json::Value` into a `rhai::Dynamic`. Done
/// manually instead of relying on serde-rhai's roundtrip because
/// we want predictable behavior: ints stay ints, floats stay
/// floats, maps become rhai object maps with string keys.
fn json_to_dynamic(v: &serde_json::Value) -> Dynamic {
    match v {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::from(n.to_string())
            }
        }
        serde_json::Value::String(s) => Dynamic::from(s.clone()),
        serde_json::Value::Array(arr) => {
            let dynamic_arr: rhai::Array = arr.iter().map(json_to_dynamic).collect();
            Dynamic::from(dynamic_arr)
        }
        serde_json::Value::Object(map) => {
            let mut dynamic_map = rhai::Map::new();
            for (k, v) in map {
                dynamic_map.insert(k.clone().into(), json_to_dynamic(v));
            }
            Dynamic::from(dynamic_map)
        }
    }
}

/// Convert a `rhai::Dynamic` back into `serde_json::Value` so the
/// node's output can land in `state` and be persisted in the trace.
fn dynamic_to_json(d: Dynamic) -> serde_json::Value {
    if d.is_unit() {
        return serde_json::Value::Null;
    }
    if let Ok(b) = d.as_bool() {
        return serde_json::Value::Bool(b);
    }
    if let Ok(i) = d.as_int() {
        return serde_json::Value::Number(i.into());
    }
    if let Ok(f) = d.as_float() {
        return serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    if d.is_string() {
        return serde_json::Value::String(d.into_string().unwrap_or_default());
    }
    if d.is_array() {
        if let Ok(arr) = d.into_array() {
            return serde_json::Value::Array(arr.into_iter().map(dynamic_to_json).collect());
        }
        return serde_json::Value::Null;
    }
    if d.is_map() {
        if let Some(map) = d.try_cast::<rhai::Map>() {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.to_string(), dynamic_to_json(v));
            }
            return serde_json::Value::Object(out);
        }
        return serde_json::Value::Null;
    }
    serde_json::Value::Null
}

fn eval_bool(expr: &str, scope: &mut Scope<'static>) -> Result<bool, String> {
    let engine = make_engine();
    engine
        .eval_expression_with_scope::<Dynamic>(scope, expr)
        .map_err(|e| e.to_string())
        .and_then(|d| {
            d.as_bool()
                .map_err(|t| format!("expression returned {t}, expected bool"))
        })
}

fn eval_value(expr: &str, scope: &mut Scope<'static>) -> Result<serde_json::Value, String> {
    let engine = make_engine();
    let dyn_val = engine
        .eval_expression_with_scope::<Dynamic>(scope, expr)
        .map_err(|e| e.to_string())?;
    Ok(dynamic_to_json(dyn_val))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation_agent::{
        AskAgentError, AutomationsAgentPool, ExitToolCall, StubAgentInvoker,
    };
    use execlaw_core::Database;
    use execlaw_core::automation_bus::{BusEventStore, Event as BusEvent};
    use execlaw_core::automations::{
        AutomationDef, AutomationStore, AutomationUpsert, EdgeDef, NodeDef, NodeKind, TriggerDef,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    /// Pool used by tests that don't exercise AskAgent. Calling
    /// `invoke` on it returns an error Ã¢â‚¬â€ fine, no test reaches that
    /// path. Keeps the runtime signature uniform.
    fn noop_pool() -> AutomationsAgentPool {
        AutomationsAgentPool::new(Arc::new(StubAgentInvoker::err(
            "noop pool Ã¢â‚¬â€ test should not exercise AskAgent",
        )))
    }

    /// Executor context for tests that don't exercise CallPlugin Ã¢â‚¬â€
    /// i.e., the default for Filter/Transform/Branch/Terminal/Notify
    /// flows. `Notify` writes through the wired DB; CallPlugin tests
    /// supply a plugin host via [`ExecutorContext::new`] directly.
    fn noop_ctx(db: &Database) -> ExecutorContext {
        ExecutorContext::new(db.clone(), noop_pool(), None)
    }

    /// Context with a specific agent pool wired (for AskAgent tests).
    fn agent_ctx(db: &Database, pool: AutomationsAgentPool) -> ExecutorContext {
        ExecutorContext::new(db.clone(), pool, None)
    }

    fn stub_pool(call: ExitToolCall) -> AutomationsAgentPool {
        AutomationsAgentPool::new(Arc::new(StubAgentInvoker::ok(call)))
    }

    fn seed_bus_event(db: &Database, id: &str, payload: serde_json::Value) -> BusEventRow {
        let store = BusEventStore::new(db);
        let evt = BusEvent {
            id: id.into(),
            kind: "webhook.received".to_owned(),
            source: "test".into(),
            received_at: 100,
            payload,
            envelope: None,
        };
        store.publish(&evt, false).unwrap();
        store.get(id).unwrap().unwrap()
    }

    #[test]
    fn rhai_eval_bool_simple() {
        let mut scope = Scope::new();
        scope.push_dynamic("x", Dynamic::from(5_i64));
        assert!(eval_bool("x > 3", &mut scope).unwrap());
        assert!(!eval_bool("x < 3", &mut scope).unwrap());
    }

    #[test]
    fn rhai_eval_bool_with_event_payload() {
        let event = serde_json::json!({
            "id": "e1",
            "kind": "webhook.received",
            "source": "ring",
            "payload": {"zone": "driveway", "confidence": 0.92},
        });
        let mut scope = build_scope_with_event(&event);
        assert!(eval_bool(r#"event.payload.zone == "driveway""#, &mut scope).unwrap());
        assert!(eval_bool("event.payload.confidence > 0.5", &mut scope).unwrap());
    }

    #[test]
    fn rhai_can_read_event_envelope_and_internal_flag() {
        // Audit fix #2: envelope must be reachable from trigger.when /
        // edge.when. Without this, flows can't gate on sender trust,
        // origin kind, or the "did another flow emit this?" signal.
        let event = serde_json::json!({
            "id": "e1",
            "kind": "web.prompt.submitted",
            "source": "web",
            "payload": {},
            "envelope": {
                "origin": {"kind": "web_socket_session", "session_id": "sess-1"},
                "identity": {"kind": "principal", "id": "p_op", "trust": "controller"},
                "correlation_id": "corr-1",
                "parent_event_id": null,
            },
            "internal": false,
        });
        let mut scope = build_scope_with_event(&event);
        assert!(
            eval_bool(r#"event.envelope.origin.kind == "web_socket_session""#, &mut scope)
                .unwrap()
        );
        assert!(
            eval_bool(r#"event.envelope.identity.trust == "controller""#, &mut scope).unwrap()
        );
        assert!(eval_bool("event.internal == false", &mut scope).unwrap());
    }

    #[test]
    fn event_context_exposes_envelope_field() {
        // Construct a real BusEventRow and verify the JSON we hand
        // Rhai actually contains an "envelope" key. Guards against
        // someone deleting the line in event_context().
        use execlaw_core::event_envelope::EventEnvelope;
        let evt = BusEventRow {
            id: "e1".into(),
            kind: "web.prompt.submitted".into(),
            source: "web".into(),
            received_at: 0,
            payload: serde_json::json!({}),
            internal: false,
            dispatched_at: None,
            envelope: EventEnvelope::system_internal(),
        };
        let ctx = event_context(&evt);
        assert!(ctx.get("envelope").is_some(), "event_context() must include envelope");
        assert!(ctx.get("internal").is_some(), "event_context() must include internal");
        assert_eq!(
            ctx["envelope"]["identity"]["kind"], "system",
            "system_internal() should serialize identity.kind = system"
        );
    }

    #[test]
    fn rhai_eval_value_returns_serializable_object() {
        let event = serde_json::json!({"payload": {"a": 1, "b": 2}});
        let mut scope = build_scope_with_event(&event);
        let v = eval_value(r#"#{ sum: event.payload.a + event.payload.b }"#, &mut scope).unwrap();
        assert_eq!(v, serde_json::json!({"sum": 3}));
    }

    #[test]
    fn rhai_eval_returns_error_for_bad_expression() {
        let mut scope = Scope::new();
        assert!(eval_bool("this is not valid rhai 1 + +", &mut scope).is_err());
    }

    /// Build a definition: trigger Ã¢â€ â€™ filter Ã¢â€ â€™ terminal.
    fn def_filter_pass(filter_expr: &str) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "f1".into(),
                    kind: NodeKind::Filter,
                    config: serde_json::json!({"expr": filter_expr}),
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "f1".into(),
                    when: None,
                },
                EdgeDef {
                    from: "f1".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn execute_filter_pass_runs_to_success() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({"x": 5}));

        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "pass".into(),
                    enabled: true,
                    definition: def_filter_pass("event.payload.x > 0"),
                },
                1000,
            )
            .unwrap();

        run_matching_automations(&noop_ctx(&db), &evt);

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Success);
        // Trace records the filter step.
        assert!(runs[0].step_traces.iter().any(|t| t.node_id == "f1"));
    }

    #[test]
    fn execute_filter_drop_marks_run_skipped() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({"x": -1}));

        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "drop".into(),
                    enabled: true,
                    definition: def_filter_pass("event.payload.x > 0"),
                },
                1000,
            )
            .unwrap();

        run_matching_automations(&noop_ctx(&db), &evt);

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Skipped);
    }

    #[test]
    fn trigger_when_predicate_filters_events() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);

        // Two events: one with source=ring, one with source=slack.
        let store = BusEventStore::new(&db);
        for (id, source) in [("e-ring", "ring"), ("e-slack", "slack")] {
            store
                .publish(
                    &BusEvent {
                        id: id.into(),
                        kind: "webhook.received".to_owned(),
                        source: source.into(),
                        received_at: 100,
                        payload: serde_json::json!({}),
                        envelope: None,
                    },
                    false,
                )
                .unwrap();
        }

        // Automation only matches ring.
        let mut def = def_filter_pass("true");
        def.trigger.when = Some(r#"event.source == "ring""#.into());
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "ring-only".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        for id in ["e-ring", "e-slack"] {
            let evt = store.get(id).unwrap().unwrap();
            run_matching_automations(&noop_ctx(&db), &evt);
        }
        let _ = run_store;

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        // Only the ring event should have produced a run.
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].event_id, "e-ring");
    }

    #[test]
    fn transform_node_output_visible_to_downstream_edges() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({"n": 10}));

        // trigger Ã¢â€ â€™ transform (doubles event.payload.n) Ã¢â€ â€™ branch on result
        // Ã¢â€ â€™ terminal-a if doubled > 15 else terminal-b
        let def = AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "double".into(),
                    kind: NodeKind::Transform,
                    config: serde_json::json!({"expr": "#{ doubled: event.payload.n * 2 }"}),
                    position: None,
                },
                NodeDef {
                    id: "branch".into(),
                    kind: NodeKind::Branch,
                    config: serde_json::json!({}),
                    position: None,
                },
                NodeDef {
                    id: "big".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
                NodeDef {
                    id: "small".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "double".into(),
                    when: None,
                },
                EdgeDef {
                    from: "double".into(),
                    to: "branch".into(),
                    when: None,
                },
                EdgeDef {
                    from: "branch".into(),
                    to: "big".into(),
                    when: Some("double.doubled > 15".into()),
                },
                EdgeDef {
                    from: "branch".into(),
                    to: "small".into(),
                    when: None, // default
                },
            ],
        };
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "branchy".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        run_matching_automations(&noop_ctx(&db), &evt);
        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Success);
        // event.payload.n = 10, doubled = 20 > 15, so we hit `big` not `small`.
        let trace_ids: Vec<_> = runs[0]
            .step_traces
            .iter()
            .map(|t| t.node_id.clone())
            .collect();
        assert!(trace_ids.contains(&"big".to_string()));
        assert!(!trace_ids.contains(&"small".to_string()));
        // Check the transform output landed in state for the edge predicate.
        let double_trace = runs[0]
            .step_traces
            .iter()
            .find(|t| t.node_id == "double")
            .unwrap();
        assert_eq!(double_trace.output, serde_json::json!({"doubled": 20}));
    }

    #[test]
    fn missing_filter_expr_fails_the_run() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({}));

        let mut def = def_filter_pass("true");
        def.nodes[0].config = serde_json::json!({}); // missing expr
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "missing-expr".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        run_matching_automations(&noop_ctx(&db), &evt);
        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Failed);
        let trace = &runs[0].step_traces[0];
        assert!(
            trace
                .error
                .as_deref()
                .unwrap_or("")
                .contains("missing config.expr")
        );
    }

    #[test]
    fn disabled_automation_is_not_run() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({"x": 5}));

        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "off".into(),
                    enabled: false,
                    definition: def_filter_pass("true"),
                },
                1000,
            )
            .unwrap();

        run_matching_automations(&noop_ctx(&db), &evt);
        assert!(
            run_store
                .list_for_automation(&row.id, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rhai_sandbox_blocks_runaway_loops() {
        let mut scope = Scope::new();
        // Multiplicative cost expression Ã¢â‚¬â€ the operator-set limit
        // (`set_max_operations`) should abort this.
        let result = eval_value("let s = 0; for i in 0..1_000_000 { s += i; } s", &mut scope);
        assert!(result.is_err(), "runaway expression must be rejected");
    }

    /// End-to-end: publish through the live `AutomationBus` (real
    /// dispatcher + poller + handler) and verify the matcher fires
    /// the configured automation and persists a run. This is the
    /// integration test that validates M1 + M2 wired together.
    #[tokio::test]
    async fn end_to_end_publish_triggers_matcher_and_runs_automation() {
        use crate::automation_bus::AutomationBus;
        use std::time::Duration;
        use tokio::sync::Notify;

        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);

        // Define + save an automation triggered by webhook events
        // from the "ring" source, with a transform that records
        // event.payload.zone into its output.
        let mut def = def_filter_pass("event.payload.zone == \"driveway\"");
        // Replace filter with transform so we can also assert on
        // output.
        def.nodes[0] = NodeDef {
            id: "f1".into(),
            kind: NodeKind::Transform,
            config: serde_json::json!({"expr": "#{ zone: event.payload.zone }"}),
            position: None,
        };
        def.trigger.when = Some(r#"event.source == "ring""#.into());
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "ring-zone".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        // Spawn the bus with the REAL matcher handler.
        let stop = std::sync::Arc::new(Notify::new());
        let (bus, tasks) =
            AutomationBus::spawn(db.clone(), build_handler(noop_ctx(&db)), stop.clone());

        // Publish a matching event through the bus.
        let evt = BusEvent {
            id: "e-ring-1".into(),
            kind: "webhook.received".to_owned(),
            source: "ring".into(),
            received_at: 100,
            payload: serde_json::json!({"zone": "driveway"}),
            envelope: None,
        };
        bus.publish(evt).await.unwrap();

        // Poll for the run row Ã¢â‚¬â€ handler runs on spawn_blocking, so
        // we allow a generous deadline.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut found = None;
        while std::time::Instant::now() < deadline {
            let runs = run_store.list_for_automation(&row.id, 10).unwrap();
            if !runs.is_empty() {
                found = Some(runs);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let runs = found.expect("expected the matcher to produce a run within 3s");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Success);
        // The Transform node's output must have made it to the trace.
        let f1 = runs[0]
            .step_traces
            .iter()
            .find(|t| t.node_id == "f1")
            .expect("transform step trace");
        assert_eq!(f1.output, serde_json::json!({"zone": "driveway"}));

        stop.notify_waiters();
        tasks.join().await;
    }

    fn ask_agent_def_two_outcomes() -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "ask".into(),
                    kind: NodeKind::AskAgent,
                    config: serde_json::json!({
                        "prompt": "Decide notify or ignore",
                        "attachments": [],
                        "reasoning_tools": [],
                        "exit_tools": [
                            {
                                "name": "notify",
                                "description": "Call on detection",
                                "args_schema": {"type": "object"}
                            },
                            {
                                "name": "ignore",
                                "description": "Call otherwise",
                                "args_schema": {"type": "object"}
                            }
                        ]
                    }),
                    position: None,
                },
                NodeDef {
                    id: "did-notify".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
                NodeDef {
                    id: "did-ignore".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "ask".into(),
                    when: None,
                },
                EdgeDef {
                    from: "ask".into(),
                    to: "did-notify".into(),
                    when: Some(r#"ask.tool == "notify""#.into()),
                },
                EdgeDef {
                    from: "ask".into(),
                    to: "did-ignore".into(),
                    when: None, // fallback
                },
            ],
        }
    }

    #[tokio::test]
    async fn ask_agent_notify_outcome_routes_to_notify_branch() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e-notify", serde_json::json!({}));

        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "stub-notify".into(),
                    enabled: true,
                    definition: ask_agent_def_two_outcomes(),
                },
                1000,
            )
            .unwrap();

        // Stub the pool to ALWAYS return `notify`.
        let pool = stub_pool(ExitToolCall {
            name: "notify".into(),
            args: serde_json::json!({"species": "cat", "confidence": 0.91}),
        });
        // Drive the executor synchronously via spawn_blocking (matches
        // production code path).
        let db_for_blocking = db.clone();
        let evt_for_blocking = evt.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = agent_ctx(&db_for_blocking, pool);
            run_matching_automations(&ctx, &evt_for_blocking);
        })
        .await
        .unwrap();

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Success);
        let trace_ids: Vec<_> = runs[0]
            .step_traces
            .iter()
            .map(|t| t.node_id.clone())
            .collect();
        assert!(trace_ids.contains(&"did-notify".to_string()));
        assert!(!trace_ids.contains(&"did-ignore".to_string()));
        // The AskAgent step's output carries the chosen tool + args
        // for downstream nodes to read.
        let ask_trace = runs[0]
            .step_traces
            .iter()
            .find(|t| t.node_id == "ask")
            .unwrap();
        assert_eq!(ask_trace.output["tool"], "notify");
        assert_eq!(ask_trace.output["args"]["species"], "cat");
    }

    #[tokio::test]
    async fn ask_agent_ignore_outcome_routes_to_fallback_branch() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e-ignore", serde_json::json!({}));

        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "stub-ignore".into(),
                    enabled: true,
                    definition: ask_agent_def_two_outcomes(),
                },
                1000,
            )
            .unwrap();

        let pool = stub_pool(ExitToolCall {
            name: "ignore".into(),
            args: serde_json::json!({}),
        });
        let db_for_blocking = db.clone();
        let evt_for_blocking = evt.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = agent_ctx(&db_for_blocking, pool);
            run_matching_automations(&ctx, &evt_for_blocking);
        })
        .await
        .unwrap();

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        let trace_ids: Vec<_> = runs[0]
            .step_traces
            .iter()
            .map(|t| t.node_id.clone())
            .collect();
        assert!(trace_ids.contains(&"did-ignore".to_string()));
        assert!(!trace_ids.contains(&"did-notify".to_string()));
    }

    #[tokio::test]
    async fn ask_agent_invoker_error_marks_run_failed_with_clear_message() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({}));

        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "stub-err".into(),
                    enabled: true,
                    definition: ask_agent_def_two_outcomes(),
                },
                1000,
            )
            .unwrap();

        // Pool that always errors.
        let pool =
            AutomationsAgentPool::new(Arc::new(StubAgentInvoker::err("simulated llm timeout")));
        let db_for_blocking = db.clone();
        let evt_for_blocking = evt.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = agent_ctx(&db_for_blocking, pool);
            run_matching_automations(&ctx, &evt_for_blocking);
        })
        .await
        .unwrap();

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Failed);
        let ask_trace = runs[0]
            .step_traces
            .iter()
            .find(|t| t.node_id == "ask")
            .unwrap();
        assert!(
            ask_trace
                .error
                .as_deref()
                .unwrap_or("")
                .contains("simulated llm timeout"),
            "error message must surface invoker failure verbatim",
        );
    }

    #[test]
    fn render_template_substitutes_dotted_paths() {
        let mut state = HashMap::new();
        state.insert(
            "event".to_string(),
            serde_json::json!({
                "id": "evt-1",
                "payload": {"zone": "driveway", "n": 7, "active": true}
            }),
        );
        assert_eq!(
            render_template("zone={{event.payload.zone}}", &state),
            "zone=driveway"
        );
        assert_eq!(render_template("n={{event.payload.n}}", &state), "n=7");
        assert_eq!(
            render_template("on={{event.payload.active}}", &state),
            "on=true"
        );
        assert_eq!(
            render_template("id={{ event.id }} n={{event.payload.n}}", &state),
            "id=evt-1 n=7"
        );
    }

    #[test]
    fn render_template_preserves_unresolved_paths() {
        let mut state = HashMap::new();
        state.insert("event".to_string(), serde_json::json!({"id": "x"}));
        // Unknown root.
        assert_eq!(render_template("hi {{nope.x}}", &state), "hi {{nope.x}}");
        // Known root, unknown leaf.
        assert_eq!(
            render_template("hi {{event.missing}}", &state),
            "hi {{event.missing}}"
        );
    }

    #[test]
    fn render_template_handles_unterminated_braces() {
        let state = HashMap::new();
        // Unterminated `{{` must not crash; we just preserve the input.
        assert_eq!(render_template("hi {{ event.x", &state), "hi {{ event.x");
        assert_eq!(render_template("plain text", &state), "plain text");
    }

    #[tokio::test]
    async fn ask_agent_prompt_templating_substitutes_event_payload() {
        // Verifies the Ring-style usage: prompt + attachments are
        // rendered against state at execute time. We use a stub
        // invoker that captures the rendered request so the
        // assertion is direct.
        use crate::automation_agent::{AgentInvoker, AskAgentRequest};
        use async_trait::async_trait;
        use std::sync::Mutex;

        struct CapturingInvoker {
            captured: Arc<Mutex<Option<AskAgentRequest>>>,
        }
        #[async_trait]
        impl AgentInvoker for CapturingInvoker {
            async fn invoke(&self, req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError> {
                *self.captured.lock().unwrap() = Some(req.clone());
                Ok(ExitToolCall {
                    name: "notify".into(),
                    args: serde_json::json!({}),
                })
            }
        }
        let captured = Arc::new(Mutex::new(None));
        let pool = AutomationsAgentPool::new(Arc::new(CapturingInvoker {
            captured: captured.clone(),
        }));

        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let evt = seed_bus_event(
            &db,
            "e-tmpl",
            serde_json::json!({
                "zone": "driveway",
                "image_url": "data:image/png;base64,AAAA"
            }),
        );
        let def = AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "ask".into(),
                    kind: NodeKind::AskAgent,
                    config: serde_json::json!({
                        "prompt": "Animal in {{event.payload.zone}}?",
                        "attachments": ["{{event.payload.image_url}}"],
                        "exit_tools": [
                            {"name": "notify", "description": "yes", "args_schema": {"type":"object"}},
                            {"name": "ignore", "description": "no",  "args_schema": {"type":"object"}}
                        ]
                    }),
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "ask".into(),
                    when: None,
                },
                EdgeDef {
                    from: "ask".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        };
        auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "tmpl".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        let db_for_blocking = db.clone();
        let evt_for_blocking = evt.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = agent_ctx(&db_for_blocking, pool);
            run_matching_automations(&ctx, &evt_for_blocking);
        })
        .await
        .unwrap();

        let req = captured
            .lock()
            .unwrap()
            .clone()
            .expect("invoker should have been called");
        assert_eq!(req.config.prompt, "Animal in driveway?");
        assert_eq!(
            req.config.attachments,
            vec!["data:image/png;base64,AAAA".to_string()]
        );
    }

    #[tokio::test]
    async fn ring_use_case_with_text_only_model_fails_with_clear_error() {
        // The locked Ring use case: webhook with image Ã¢â€ â€™ AskAgent
        // with attachments Ã¢â€ â€™ text-only model in inference resolver Ã¢â€ â€™
        // VisionRequiredButTextOnlyModel surfaces in the run trace
        // with operator-actionable guidance.
        use crate::automation_agent::InferenceAgentInvoker;
        use execlaw_inference_api::InferenceClient;

        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(
            &db,
            "e-ring",
            serde_json::json!({"image_url": "data:image/png;base64,iVBORw0KGgo"}),
        );

        // Ring-style definition: pass the event's image through the
        // attachments list. The capability check fires before any
        // HTTP request, so the URL is dummy.
        let def = AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "ask".into(),
                    kind: NodeKind::AskAgent,
                    config: serde_json::json!({
                        "prompt": "Is there an animal in this image?",
                        // The locked design routes attachments through
                        // payload templating; M3a hardcodes the path
                        // for the capability-check test.
                        "attachments": ["data:image/png;base64,iVBORw0KGgo"],
                        "exit_tools": [
                            {"name": "notify", "description": "Animal", "args_schema": {"type":"object"}},
                            {"name": "ignore", "description": "No animal", "args_schema": {"type":"object"}}
                        ]
                    }),
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "ask".into(),
                    when: None,
                },
                EdgeDef {
                    from: "ask".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        };
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "ring".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        // Configure a resolver that returns the current text-only
        // default model Ã¢â‚¬â€ the capability check should reject the
        // request before the bogus URL is ever contacted.
        let bootstrap = Arc::new(InferenceClient::new("http://127.0.0.1:1"));
        let mut resolver = crate::inference_resolver::InferenceResolver::new(Some(bootstrap));
        resolver.bootstrap_model = Some("QuantTrio/Qwen3.5-27B-AWQ".into());
        let pool = AutomationsAgentPool::new(Arc::new(InferenceAgentInvoker::new(
            db.clone(),
            Arc::new(resolver),
        )));

        let db_for_blocking = db.clone();
        let evt_for_blocking = evt.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = agent_ctx(&db_for_blocking, pool);
            run_matching_automations(&ctx, &evt_for_blocking);
        })
        .await
        .unwrap();

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Failed);
        let ask_trace = runs[0]
            .step_traces
            .iter()
            .find(|t| t.node_id == "ask")
            .unwrap();
        let err = ask_trace.error.as_deref().unwrap_or("");
        assert!(
            err.contains("vision") && err.contains("text-only"),
            "vision-required error message must surface clearly; got: {err}",
        );
    }

    #[tokio::test]
    async fn end_to_end_non_matching_event_produces_no_run() {
        use crate::automation_bus::AutomationBus;
        use std::time::Duration;
        use tokio::sync::Notify;

        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);

        // Automation only matches RoutineFired kind.
        let mut def = def_filter_pass("true");
        def.trigger.kind = "routine.fired".to_owned();
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "routine-only".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        let stop = std::sync::Arc::new(Notify::new());
        let (bus, tasks) =
            AutomationBus::spawn(db.clone(), build_handler(noop_ctx(&db)), stop.clone());

        // Publish a WebhookReceived event Ã¢â‚¬â€ wrong kind, must not trigger.
        bus.publish(BusEvent {
            id: "e-webhook".into(),
            kind: "webhook.received".to_owned(),
            source: "ring".into(),
            received_at: 100,
            payload: serde_json::json!({}),
            envelope: None,
        })
        .await
        .unwrap();

        // Give the handler time to (correctly) do nothing.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert!(runs.is_empty(), "wrong-kind event must not produce a run");

        stop.notify_waiters();
        tasks.join().await;
    }

    // ----------------------- Notify (M6) ------------------------------

    /// Build trigger Ã¢â€ â€™ notify Ã¢â€ â€™ terminal.
    fn def_notify(notify_cfg: serde_json::Value) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "alert".into(),
                    kind: NodeKind::Notify,
                    config: notify_cfg,
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "alert".into(),
                    when: None,
                },
                EdgeDef {
                    from: "alert".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn notify_node_inserts_alert_row_and_run_succeeds() {
        use execlaw_core::alerts::{AlertStatus, AlertStore};

        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({"zone": "driveway"}));

        let def = def_notify(serde_json::json!({
            "title": "Motion in {{event.payload.zone}}",
            "detail": "An automation tripped",
            "severity": "Warning",
        }));
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "notify-1".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        run_matching_automations(&noop_ctx(&db), &evt);

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Success);

        let store = AlertStore::new(&db);
        let alerts = store.list(None, None).unwrap();
        assert_eq!(alerts.len(), 1);
        let a = &alerts[0];
        // Template substituted on title.
        assert_eq!(a.title, "Motion in driveway");
        assert_eq!(a.detail.as_deref(), Some("An automation tripped"));
        assert_eq!(a.status, AlertStatus::Firing);
        // Default source = `automation:<node_id>`.
        assert_eq!(a.source, "automation:alert");
    }

    #[test]
    fn notify_node_with_missing_title_fails_validation() {
        use execlaw_core::automations::{AutomationError, validate};

        let def = def_notify(serde_json::json!({})); // no title
        match validate(&def) {
            Err(AutomationError::Validation(msg)) => {
                assert!(
                    msg.contains("config.title"),
                    "expected title error, got: {msg}"
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn notify_node_with_bad_severity_fails_validation() {
        use execlaw_core::automations::{AutomationError, validate};

        let def = def_notify(serde_json::json!({
            "title": "x",
            "severity": "Catastrophic",
        }));
        match validate(&def) {
            Err(AutomationError::Validation(msg)) => {
                assert!(
                    msg.contains("severity"),
                    "expected severity error, got: {msg}"
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn notify_node_dedupes_re_firing_via_fingerprint() {
        use execlaw_core::alerts::AlertStore;

        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let evt1 = seed_bus_event(&db, "e1", serde_json::json!({"zone": "driveway"}));
        let evt2 = seed_bus_event(&db, "e2", serde_json::json!({"zone": "driveway"}));

        let def = def_notify(serde_json::json!({
            "title": "Motion in {{event.payload.zone}}",
        }));
        auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "notify-dup".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        // Same fingerprint Ã¢â€ â€™ second firing bumps occurrence_count, no
        // new row.
        run_matching_automations(&noop_ctx(&db), &evt1);
        run_matching_automations(&noop_ctx(&db), &evt2);

        let alerts = AlertStore::new(&db).list(None, None).unwrap();
        assert_eq!(
            alerts.len(),
            1,
            "dedup must collapse same-fingerprint firings"
        );
        assert_eq!(alerts[0].occurrence_count, 2);
    }

    // ----------------------- CallPlugin (M6) ---------------------------

    /// Build trigger Ã¢â€ â€™ call_plugin Ã¢â€ â€™ terminal.
    fn def_call_plugin(cfg: serde_json::Value) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "call".into(),
                    kind: NodeKind::CallPlugin,
                    config: cfg,
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "call".into(),
                    when: None,
                },
                EdgeDef {
                    from: "call".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        }
    }

    #[tokio::test]
    async fn call_plugin_without_plugin_host_surfaces_clear_error() {
        let db = fresh_db();
        let auto_store = AutomationStore::new(&db);
        let run_store = AutomationRunStore::new(&db);
        let evt = seed_bus_event(&db, "e1", serde_json::json!({}));

        let def = def_call_plugin(serde_json::json!({
            "tool": "signal.send_message",
            "args": {"to": "+15551234", "body": "hello"},
        }));
        let row = auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "call-no-host".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();

        // noop_ctx wires plugin_host = None Ã¢â‚¬â€ the CallPlugin executor
        // must turn that into a per-node error (not a panic).
        let db2 = db.clone();
        let evt2 = evt.clone();
        tokio::task::spawn_blocking(move || run_matching_automations(&noop_ctx(&db2), &evt2))
            .await
            .unwrap();

        let runs = run_store.list_for_automation(&row.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Failed);
        let trace = &runs[0].step_traces[0];
        assert!(
            trace
                .error
                .as_deref()
                .unwrap_or("")
                .contains("no plugin host"),
            "expected plugin-host error, got: {:?}",
            trace.error
        );
    }

    #[test]
    fn call_plugin_with_empty_tool_fails_validation() {
        use execlaw_core::automations::{AutomationError, validate};

        let def = def_call_plugin(serde_json::json!({"tool": ""}));
        match validate(&def) {
            Err(AutomationError::Validation(msg)) => {
                assert!(msg.contains("config.tool"), "got: {msg}");
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn call_plugin_with_non_object_args_fails_validation() {
        use execlaw_core::automations::{AutomationError, validate};

        let def = def_call_plugin(serde_json::json!({"tool": "x", "args": "not-an-object"}));
        match validate(&def) {
            Err(AutomationError::Validation(msg)) => {
                assert!(msg.contains("args"), "got: {msg}");
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn render_template_in_value_substitutes_string_leaves_recursively() {
        let mut state = HashMap::new();
        state.insert(
            "event".to_string(),
            serde_json::json!({"payload": {"to": "+15551234"}}),
        );
        let v = serde_json::json!({
            "to": "{{event.payload.to}}",
            "body": "hi",
            "meta": {"src": "auto:{{event.payload.to}}"},
            "tags": ["a", "{{event.payload.to}}"],
            "n": 7,
        });
        let out = render_template_in_value(&v, &state);
        assert_eq!(
            out,
            serde_json::json!({
                "to": "+15551234",
                "body": "hi",
                "meta": {"src": "auto:+15551234"},
                "tags": ["a", "+15551234"],
                "n": 7,
            })
        );
    }
}
