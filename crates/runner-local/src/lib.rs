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

#![forbid(unsafe_code)]

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
