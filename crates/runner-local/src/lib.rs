//! execlaw-runner-local
//!
//! The one runner. Speaks OpenAI-compatible API (via
//! `execlaw_inference_api`) to whichever local backend the operator has
//! configured in `config_runner_deployments`.
//!
//! Phase 0 ships a stub. The session-hydration, tool-dispatch,
//! compaction, and per-turn capability-token-bound call paths land in
//! Phase 1 (see §Phase 1 in MIGRATION_PLAN.md).
//!
//! **No cloud-vendor SDKs. Ever.** See §0 axiom #1.
//!
//! 2026-04-29 — the legacy `memory_tool` / `thread_tool` modules
//! moved to `execlaw_core::builtin_tools` as first-class
//! `ToolImpl`-based built-ins registered through the host's
//! `HookRegistry`. The dispatch layer in `execlaw_server` now reaches
//! them via the registry, so the duplicate `MemoryToolDispatcher` /
//! `ThreadToolDispatcher` types here were pruned.

#![forbid(unsafe_code)]

pub mod turn;

use execlaw_inference_api::InferenceClient;

/// Minimal runner handle. Phase 1 turns this into a real trait that
/// `session` invokes per turn.
#[derive(Debug, Clone)]
pub struct RunnerLocal {
    pub inference: InferenceClient,
}

impl RunnerLocal {
    pub fn new(inference: InferenceClient) -> Self {
        Self { inference }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_can_be_constructed() {
        let r = RunnerLocal::new(InferenceClient::new("http://127.0.0.1:8000/v1"));
        assert_eq!(r.inference.base_url, "http://127.0.0.1:8000/v1");
    }
}
