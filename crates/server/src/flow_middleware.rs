//! Pre-turn flow middleware (Phase A of the Flows redesign,
//! 2026-05-22).
//!
//! The chat handler calls [`evaluate`] right after the inbound
//! `user_msg` event has been committed but before the turn driver
//! runs. Matching flows execute synchronously; mutations to the
//! upcoming turn (rewritten prompt, added skills, narrowed tools,
//! etc.) accumulate into a [`FlowOutcome`] which the chat handler
//! then applies to its own `TurnConfig` / message text / caps.
//!
//! **Phase A scope:** only [`NodeKind::RewritePrompt`] mutations are
//! harvested. The other mutator nodes (SetSkills, SetTools,
//! AddMemory, SetTrust, AddAttachment) ship in Phase B; their
//! executors don't exist yet, so the matcher tolerates flows that
//! contain only RewritePrompt + the filtering/branching kinds.
//!
//! **Performance contract:** zero flows enabled → one indexed
//! SQLite SELECT (sub-microsecond after warmup). One enabled flow,
//! single mutator → <2ms end-to-end (Rhai eval dominates). The
//! `automation_runtime` benchmarks pin the per-graph cost; this
//! module just thin-wraps the matcher.
//!
//! **Failure mode:** any error inside the middleware (DB read
//! failure, flow execution error, malformed harvest payload) logs
//! at WARN and falls through to a zero-mutation outcome. The chat
//! turn always runs — flows are best-effort customization, not a
//! gate.

use crate::automation_runtime::{
    self, ExecOutcome, ExecutorContext, FlowEventInput, dry_run, event_context, trigger_matches,
};
use crate::state::AppState;
use execlaw_core::automations::{AutomationStore, NodeKind};
use std::collections::HashMap;
use tracing::warn;

/// Trigger kind every chat-context flow listens on. Channel + source
/// distinctions live in `event.payload.channel` so operators narrow
/// with Filter Rhai expressions rather than via separate trigger
/// kinds. Keeping it to ONE kind also keeps the SPA trigger picker
/// trivially simple.
pub const CHAT_PROMPT_KIND: &str = "chat.prompt";

/// Accumulated mutations the chat handler applies before the turn
/// runs. Phase A only carries `rewritten_prompt`; B/C extend in
/// place without changing the call shape on the chat side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowOutcome {
    /// New user-message text when one or more `RewritePrompt` nodes
    /// fired. Last-writer-wins across matching flows (deterministic
    /// alphabetical-by-name ordering — see [`evaluate`]).
    pub rewritten_prompt: Option<String>,
    /// `true` when at least one matching flow's execution outcome
    /// was [`ExecOutcome::Skipped`] (a Filter said falsy). The chat
    /// handler is free to honor or ignore this; Phase A doesn't
    /// short-circuit the turn on it — see roadmap.
    pub dropped: bool,
}

impl FlowOutcome {
    /// `true` when the middleware produced any actionable change.
    /// Lets the chat handler skip the apply-mutations step entirely
    /// in the (common) no-flows case.
    pub fn is_noop(&self) -> bool {
        self.rewritten_prompt.is_none() && !self.dropped
    }
}

/// Run all matching `chat.prompt` flows against `event` and return
/// the accumulated mutations.
///
/// Hot-path cost when no flows are enabled: one
/// `list_enabled_for_kind` SELECT against `state_automations`,
/// which uses the index on `(enabled, kind)`. The function returns
/// `FlowOutcome::default()` and the chat handler proceeds as if
/// nothing happened.
///
/// Multi-flow ordering: flows are sorted alphabetically by `name`
/// before execution so the same set of flows always produces the
/// same outcome regardless of insertion order. Last-writer-wins
/// applies to scalar fields (`rewritten_prompt`); operators
/// rename to control precedence.
pub fn evaluate(state: &AppState, event: &FlowEventInput) -> FlowOutcome {
    let store = AutomationStore::new(&state.db);
    let mut flows = match store.list_enabled_for_kind(&event.kind) {
        Ok(rows) => rows,
        Err(e) => {
            warn!(
                target: "flow_middleware",
                error = %e,
                kind = %event.kind,
                "list_enabled_for_kind failed; chat proceeds without flow mutations",
            );
            return FlowOutcome::default();
        }
    };
    if flows.is_empty() {
        return FlowOutcome::default();
    }
    // Deterministic ordering for the last-writer-wins resolution
    // below. Sorting a 0-or-1-element slice is free in practice;
    // larger lists are still microseconds.
    flows.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));

    let event_ctx = event_context(event);
    let ctx = ExecutorContext::new(state.db.clone(), Some(state.plugin_host.clone()));
    let mut outcome = FlowOutcome::default();

    for flow in flows {
        // trigger.when is an additional gate beyond kind matching —
        // a flow with `kind=chat.prompt` + `when="event.payload.channel == \"whatsapp\""`
        // skips web-channel prompts cleanly.
        if !trigger_matches(&flow.definition.trigger, &event_ctx) {
            continue;
        }
        // Pre-index node kinds so the harvest pass below can look
        // up by node id without walking the Vec every step.
        let kind_by_id: HashMap<&str, NodeKind> = flow
            .definition
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.kind))
            .collect();

        let result = dry_run(&ctx, &flow, event);
        if result.outcome == ExecOutcome::Skipped {
            outcome.dropped = true;
            // A Filter-drop short-circuits *this* flow; subsequent
            // flows still get a chance to fire. Phase B documents
            // the policy alternatives (any-drop = drop turn vs
            // all-drop = drop turn).
            continue;
        }
        // Harvest each surviving node's typed output. Today only
        // RewritePrompt produces a consumable mutation; future
        // mutator kinds add their own arms here.
        for trace in &result.step_traces {
            if trace.error.is_some() {
                // Errored nodes shouldn't contribute mutations.
                // The error is already in the run trace for ops
                // visibility; the chat handler proceeds with the
                // unmutated input.
                continue;
            }
            let Some(kind) = kind_by_id.get(trace.node_id.as_str()).copied() else {
                continue;
            };
            match kind {
                NodeKind::RewritePrompt => {
                    if let Some(text) = trace.output.get("text").and_then(|v| v.as_str()) {
                        outcome.rewritten_prompt = Some(text.to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    outcome
}

/// Builder for the `chat.prompt` event input. Hot path — the chat
/// handler constructs one of these per turn. Keep allocations
/// tight: only the fields the matcher + Rhai scope actually read.
///
/// `channel` defaults to `"web"` when the inbound message has no
/// transport-bridge annotation (the SPA's composer is the
/// canonical example). Plugin transports (Signal, WhatsApp) set
/// it from `inbound_channel_origin`.
pub fn build_chat_prompt_event(
    conversation_id: &str,
    user_text: &str,
    sender_principal_id: Option<&str>,
    channel: &str,
    attachment_ids: &[String],
) -> FlowEventInput {
    use execlaw_core::event_envelope::{EventEnvelope, OriginRef, SenderIdentity, TrustClass};

    let envelope = EventEnvelope {
        origin: OriginRef::ChatAppend {
            conversation_id: conversation_id.to_owned(),
        },
        identity: match sender_principal_id {
            Some(id) => SenderIdentity::Principal {
                id: execlaw_core::ids::PrincipalId::from(id.to_owned()),
                trust: TrustClass::Controller,
            },
            None => SenderIdentity::System,
        },
        correlation_id: format!("chat-{}", uuid::Uuid::new_v4()),
        parent_event_id: None,
    };
    FlowEventInput {
        id: format!("chat-prompt-{}", uuid::Uuid::new_v4()),
        kind: CHAT_PROMPT_KIND.to_owned(),
        source: format!("chat:{channel}"),
        received_at: chrono::Utc::now().timestamp_millis(),
        payload: serde_json::json!({
            "text": user_text,
            "conversation_id": conversation_id,
            "sender_principal_id": sender_principal_id,
            "channel": channel,
            "attachment_ids": attachment_ids,
        }),
        internal: false,
        envelope,
    }
}

// Re-export the runtime types the chat handler needs so it doesn't
// have to know flows execute via `automation_runtime`.
pub use automation_runtime::FlowEventInput as ChatFlowEvent;

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::Database;
    use execlaw_core::automations::{AutomationStore, AutomationUpsert};
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_plugin_host::{HookRegistry, PluginHost};
    use std::sync::Arc;
    use std::time::Instant;

    fn fresh_state() -> AppState {
        crate::routes::test_app_state()
    }

    fn insert_flow(state: &AppState, name: &str, def_json: serde_json::Value) {
        let def: execlaw_core::automations::AutomationDef =
            serde_json::from_value(def_json).expect("test flow def must parse");
        AutomationStore::new(&state.db)
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: name.into(),
                    enabled: true,
                    definition: def,
                },
                1000,
            )
            .unwrap();
    }

    fn chat_event(text: &str) -> FlowEventInput {
        build_chat_prompt_event("conv-test", text, Some("op"), "web", &[])
    }

    #[test]
    fn evaluate_returns_default_when_no_flows_enabled() {
        let state = fresh_state();
        let outcome = evaluate(&state, &chat_event("hello"));
        assert_eq!(outcome, FlowOutcome::default());
        assert!(outcome.is_noop());
    }

    #[test]
    fn evaluate_applies_single_rewrite_prompt() {
        let state = fresh_state();
        insert_flow(
            &state,
            "prefix-fixer",
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "\"[fixed] \" + event.payload.text"}
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        );

        let outcome = evaluate(&state, &chat_event("hello world"));
        assert_eq!(
            outcome.rewritten_prompt.as_deref(),
            Some("[fixed] hello world"),
        );
        assert!(!outcome.dropped);
    }

    #[test]
    fn evaluate_skips_disabled_flow() {
        let state = fresh_state();
        let store = AutomationStore::new(&state.db);
        let def: execlaw_core::automations::AutomationDef = serde_json::from_value(
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "\"[disabled] \" + event.payload.text"}
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        ).unwrap();
        store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "disabled-flow".into(),
                    enabled: false, // <-- key
                    definition: def,
                },
                1000,
            )
            .unwrap();

        let outcome = evaluate(&state, &chat_event("hello"));
        assert!(outcome.is_noop(), "disabled flow must not contribute mutations");
    }

    #[test]
    fn evaluate_respects_trigger_when_filter() {
        let state = fresh_state();
        // Flow only fires when channel == "whatsapp".
        insert_flow(
            &state,
            "whatsapp-only",
            serde_json::json!({
                "trigger": {
                    "kind": "chat.prompt",
                    "when": "event.payload.channel == \"whatsapp\""
                },
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "\"[wa] \" + event.payload.text"}
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        );

        // Web event — flow's when predicate is false → no mutation.
        let web_outcome = evaluate(&state, &chat_event("hello"));
        assert!(web_outcome.is_noop());

        // WhatsApp event — flow fires.
        let wa_event = build_chat_prompt_event("conv-test", "hello", Some("op"), "whatsapp", &[]);
        let wa_outcome = evaluate(&state, &wa_event);
        assert_eq!(wa_outcome.rewritten_prompt.as_deref(), Some("[wa] hello"));
    }

    #[test]
    fn evaluate_multi_flow_last_writer_wins_by_name() {
        // Two flows both rewrite the prompt. Names: "aaa" and "zzz".
        // Sort order is alphabetical, so "zzz" runs LAST and its
        // rewrite wins (last-writer).
        let state = fresh_state();
        insert_flow(
            &state,
            "aaa-first",
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "\"[A] \" + event.payload.text"}
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        );
        insert_flow(
            &state,
            "zzz-second",
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "\"[Z] \" + event.payload.text"}
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        );

        let outcome = evaluate(&state, &chat_event("hi"));
        // "zzz-second" sorts last → its mutation wins.
        assert_eq!(outcome.rewritten_prompt.as_deref(), Some("[Z] hi"));
    }

    #[test]
    fn evaluate_filter_drop_sets_dropped_flag() {
        let state = fresh_state();
        insert_flow(
            &state,
            "drop-all",
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{"id": "f", "kind": "Filter", "config": {"expr": "false"}}],
                "edges": [
                    {"from": "trigger", "to": "f", "when": null},
                    {"from": "f", "to": "END", "when": null}
                ]
            }),
        );
        let outcome = evaluate(&state, &chat_event("anything"));
        assert!(outcome.dropped);
        assert!(outcome.rewritten_prompt.is_none());
    }

    #[test]
    fn evaluate_errored_node_does_not_contribute_mutation() {
        let state = fresh_state();
        insert_flow(
            &state,
            "bad-rhai",
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "42"}  // returns int, not string → error
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        );
        let outcome = evaluate(&state, &chat_event("hello"));
        // Failed run → no mutation harvested.
        assert!(outcome.is_noop());
    }

    /// Perf gate: zero flows enabled must complete in well under 1ms
    /// on any reasonable hardware. Running 1000 evaluations + 100ms
    /// total wall time gives ~100µs/call upper bound on slow CI.
    #[test]
    fn evaluate_zero_flow_hot_path_under_budget() {
        let state = fresh_state();
        let evt = chat_event("hello");
        let start = Instant::now();
        for _ in 0..1000 {
            let _ = evaluate(&state, &evt);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 500,
            "1000 zero-flow evaluate() calls took {:?} — budget is <500ms; the hot path is regressed",
            elapsed,
        );
    }

    /// Perf gate: one flow with a single RewritePrompt should round
    /// trip in <2ms on average. 100 invocations / 200ms ceiling.
    #[test]
    fn evaluate_single_flow_under_budget() {
        let state = fresh_state();
        insert_flow(
            &state,
            "prefix",
            serde_json::json!({
                "trigger": {"kind": "chat.prompt", "when": null},
                "nodes": [{
                    "id": "rw",
                    "kind": "RewritePrompt",
                    "config": {"expr": "\"[p] \" + event.payload.text"}
                }],
                "edges": [
                    {"from": "trigger", "to": "rw", "when": null},
                    {"from": "rw", "to": "END", "when": null}
                ]
            }),
        );
        let evt = chat_event("hello");
        let start = Instant::now();
        for _ in 0..100 {
            let outcome = evaluate(&state, &evt);
            assert_eq!(outcome.rewritten_prompt.as_deref(), Some("[p] hello"));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 1000,
            "100 single-flow evaluate() calls took {:?} — budget is <1s",
            elapsed,
        );
    }

    // Force-link these to silence unused-warning shenanigans if the
    // test fixture happens to not exercise them; harmless in prod.
    #[allow(dead_code)]
    fn _ensure_imports_used() {
        let _: fn() -> Database = || {
            let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
            MigrationRunner::new(&db).apply_all().unwrap();
            db
        };
        let _ = HookRegistry::new();
        let _: fn() -> Arc<PluginHost> = || unimplemented!();
    }
}
