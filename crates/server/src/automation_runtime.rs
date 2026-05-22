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

use execlaw_core::Database;
use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use execlaw_core::automation_runs::{AutomationRunStatus, AutomationRunStore, StepTrace};
use execlaw_core::automations::{
    AutomationDef, AutomationRow, AutomationStore, END_SENTINEL, NodeDef, NodeKind,
    TRIGGER_SENTINEL, TriggerDef,
};
use execlaw_core::event_envelope::EventEnvelope;
use execlaw_core::ids::AlertId;
use execlaw_plugin_host::PluginHost;
use rhai::{Dynamic, Engine, Scope};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

/// Input event to the flow executor. Replaces the prior
/// `execlaw_core::automation_bus::FlowEventInput` (deleted with the bus
/// itself in Rip 3) with just the fields the test-run path actually
/// reads. Field names + JSON shape are preserved so saved test-run
/// payloads still deserialize unchanged.
#[derive(Debug, Clone)]
pub struct FlowEventInput {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub received_at: i64,
    pub payload: serde_json::Value,
    pub internal: bool,
    pub envelope: EventEnvelope,
}

/// Per-run handles for the side-effect executors. Threaded into
/// [`execute_node`] so M4-and-beyond kinds (Notify, CallPlugin, â€¦)
/// can reach the relevant subsystem without us re-plumbing every
/// signature each time we land a new kind.
#[derive(Clone)]
pub struct ExecutorContext {
    pub db: Database,
    /// Optional so tests that don't exercise CallPlugin can wire the
    /// runtime without spinning up a full plugin host. A `None` here
    /// turns CallPlugin into a clean per-node error rather than a
    /// runtime panic.
    pub plugin_host: Option<PluginHost>,
}

impl ExecutorContext {
    pub fn new(db: Database, plugin_host: Option<PluginHost>) -> Self {
        Self {
            db,
            plugin_host,
        }
    }
}


/// Top-level matcher: list enabled automations for the event's kind,
/// filter by trigger predicate, run each that matches.
pub(crate) fn run_matching_automations(ctx: &ExecutorContext, evt: &FlowEventInput) {
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
            kind = %evt.kind,
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
fn event_context(evt: &FlowEventInput) -> serde_json::Value {
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
    evt: &FlowEventInput,
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
    let _ = (run_id, outcome);
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
    sample: &FlowEventInput,
) -> DryRunResult {
    let event_ctx = event_context(sample);
    let mut state: HashMap<String, serde_json::Value> = HashMap::new();
    state.insert("event".to_string(), event_ctx);
    let mut traces: Vec<StepTrace> = Vec::new();
    let dry_run_id = format!("dry-{}", uuid::Uuid::new_v4());
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
        let exec_result = execute_node(node, state, ctx, envelope, run_id);
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
    _envelope: &execlaw_core::event_envelope::EventEnvelope,
    _run_id: &str,
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
        NodeKind::Notify => execute_notify(node, state, &ctx.db),
        NodeKind::CallPlugin => execute_call_plugin(node, state, ctx.plugin_host.as_ref()),
        // 2026-05-22 — AskAgent + SendReply executors removed in the
        // M6 rip-out. Their NodeKind variants remain so saved flows
        // deserialize cleanly, but execution returns a clean,
        // operator-actionable error. Reintroduce as the middleware
        // redesign (pre-turn flow mutators) lands.
        NodeKind::AskAgent => NodeOutcome::Error(
            "AskAgent: not supported in this build — the M6 single-shot \
             agent invoker was removed pending the middleware redesign. \
             Use the legacy chat path or rebuild a middleware-aware \
             AskAgent."
                .into(),
        ),
        NodeKind::SendReply => NodeOutcome::Error(
            "SendReply: not supported in this build — the ReplyRouter \
             was removed in the M6 rip-out. Future flows will mutate the \
             chat turn pre-execution; SendReply is no longer the delivery \
             abstraction."
                .into(),
        ),
        _ => NodeOutcome::Error(format!(
            "node kind '{}' not implemented in this milestone",
            node.kind.as_str()
        )),
    }
    // `envelope` + `run_id` retained on the signature for future use
    // (middleware redesign), but currently consumed only by the
    // surviving Filter/Transform/Branch/Terminal/Notify/CallPlugin
    // executors that don't need them.
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
    use execlaw_core::automations::{
        AutomationStore, AutomationUpsert, EdgeDef,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::event_envelope::EventEnvelope;
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn noop_ctx(db: &Database) -> ExecutorContext {
        ExecutorContext::new(db.clone(), None)
    }

    fn sample_event(kind: &str, payload: serde_json::Value) -> FlowEventInput {
        FlowEventInput {
            id: "e-test".into(),
            kind: kind.into(),
            source: "test".into(),
            received_at: 0,
            payload,
            internal: false,
            envelope: EventEnvelope::system_internal(),
        }
    }

    #[test]
    fn rhai_eval_bool_simple() {
        let mut scope = Scope::new();
        scope.push_dynamic("x", Dynamic::from(7_i64));
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
    fn rhai_eval_returns_error_for_bad_expression() {
        let mut scope = Scope::new();
        assert!(eval_bool("this is not valid rhai 1 + +", &mut scope).is_err());
    }

    /// Filter → Terminal: trivial happy-path graph. Confirms dry_run
    /// drives the surviving executor end-to-end against the new
    /// FlowEventInput shape.
    #[test]
    fn dry_run_filter_pass_then_terminal_succeeds() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let def: AutomationDef = serde_json::from_value(serde_json::json!({
            "trigger": {"kind": "webhook.received", "when": null},
            "nodes": [
                {"id": "f1", "kind": "Filter", "config": {"expr": "true"}},
                {"id": "end", "kind": "Terminal", "config": {}},
            ],
            "edges": [
                {"from": "trigger", "to": "f1", "when": null},
                {"from": "f1", "to": "end", "when": null},
            ],
        })).unwrap();
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "test".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();
        let ctx = noop_ctx(&db);
        let evt = sample_event("webhook.received", serde_json::json!({}));
        let result = dry_run(&ctx, &row, &evt);
        assert_eq!(result.outcome, ExecOutcome::Success);
    }

    /// Filter falsy → Skipped (validates the Drop outcome path).
    #[test]
    fn dry_run_filter_drop_marks_run_skipped() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let def: AutomationDef = serde_json::from_value(serde_json::json!({
            "trigger": {"kind": "webhook.received", "when": null},
            "nodes": [
                {"id": "f1", "kind": "Filter", "config": {"expr": "false"}},
                {"id": "end", "kind": "Terminal", "config": {}},
            ],
            "edges": [
                {"from": "trigger", "to": "f1", "when": null},
                {"from": "f1", "to": "end", "when": null},
            ],
        })).unwrap();
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "test".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();
        let ctx = noop_ctx(&db);
        let evt = sample_event("webhook.received", serde_json::json!({}));
        let result = dry_run(&ctx, &row, &evt);
        assert_eq!(result.outcome, ExecOutcome::Skipped);
    }

    /// AskAgent node returns a clean "not supported" error after the
    /// M6 rip-out. Saved flows containing the variant still
    /// deserialize; execution surfaces the error in the step trace.
    #[test]
    fn ask_agent_node_returns_unsupported_error() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let def: AutomationDef = serde_json::from_value(serde_json::json!({
            "trigger": {"kind": "webhook.received", "when": null},
            "nodes": [
                {"id": "ask", "kind": "AskAgent", "config": {
                    "prompt": "x",
                    "exit_tools": [{"name": "ok", "description": "", "args_schema": {"type":"object"}}]
                }},
                {"id": "end", "kind": "Terminal", "config": {}},
            ],
            "edges": [
                {"from": "trigger", "to": "ask", "when": null},
                {"from": "ask", "to": "end", "when": null},
            ],
        })).unwrap();
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "test".into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();
        let ctx = noop_ctx(&db);
        let evt = sample_event("webhook.received", serde_json::json!({}));
        let result = dry_run(&ctx, &row, &evt);
        assert_eq!(result.outcome, ExecOutcome::Failed);
        let ask_trace = result
            .step_traces
            .iter()
            .find(|t| t.node_id == "ask")
            .unwrap();
        let err = ask_trace.error.as_deref().unwrap_or("");
        assert!(
            err.contains("AskAgent") && err.contains("not supported"),
            "AskAgent error must surface clearly; got: {err}",
        );
    }
}

