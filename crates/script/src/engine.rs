//! [`ScriptEngine`] — factory for per-plugin Rhai engines.
//!
//! Each [`crate::ScriptPlugin`] gets its own [`rhai::Engine`] so
//! the registered primitives (HTTP, cache, logging) can capture
//! per-plugin state (the plugin_id, the per-plugin cache, the
//! shared HTTP client) without the engines stepping on each other.
//! That isolation costs ~300 KB of stdlib per plugin — fine at
//! single-digit plugin counts.

use crate::cache::HttpCache;
use crate::host_caps::HostCapabilitiesArc;
use crate::primitives::{self, OwningPluginSlot};
use std::sync::Arc;
use std::time::Duration;

/// Sandbox limits — wide enough for normal HTTP-API-wrapper
/// plugins, tight enough that a runaway script can't wedge the
/// host. Tunable later via per-plugin manifest knobs if any
/// real-world plugin needs more headroom.
const MAX_OPS_PER_CALL: u64 = 1_000_000;
const MAX_CALL_DEPTH: usize = 64;
const MAX_EXPR_DEPTH: usize = 64;
const MAX_STRING_LEN: usize = 1_000_000; // 1 MB
const MAX_ARRAY_LEN: usize = 100_000;
const MAX_MAP_LEN: usize = 100_000;

#[derive(Clone)]
pub struct ScriptEngine {
    http_agent: ureq::Agent,
    /// When true, the script tier's SSRF guard accepts loopback /
    /// private / link-local destinations. **Tests only** — the
    /// production path constructs via `new()` which sets this to
    /// false. Mirrors the same flag in tool_apis_http's
    /// `HttpWebFetchApi::with_loopback_allowed`.
    allow_loopback: bool,
    /// Optional host-capabilities surface the script tier reaches
    /// when scripts call `sidecar_url` / `ws_subscribe` /
    /// `host_route_inbound`. `None` for unit-test fixtures + dev
    /// runs without an `AppState`; production passes the host
    /// crate's concrete impl in via [`with_host_capabilities`].
    /// Bindings remain registered either way; absent caps surface
    /// at call time as a clean Rhai runtime error.
    host_caps: Option<HostCapabilitiesArc>,
}

impl ScriptEngine {
    /// Construct a factory with a default sync HTTP agent. ureq is
    /// fully sync (no internal tokio runtime), safe to call from
    /// the spawn_blocking thread that runs the script.
    pub fn new() -> Self {
        Self {
            http_agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .user_agent("execlaw/script-runtime/0.1")
                .build(),
            allow_loopback: false,
            host_caps: None,
        }
    }

    pub fn with_http_agent(http_agent: ureq::Agent) -> Self {
        Self {
            http_agent,
            allow_loopback: false,
            host_caps: None,
        }
    }

    /// Plug the host-capabilities surface in. Production callers
    /// (the cli boot path) use this; tests typically don't.
    pub fn with_host_capabilities(mut self, caps: HostCapabilitiesArc) -> Self {
        self.host_caps = Some(caps);
        self
    }

    /// Test-only: skip the SSRF guard so the engine can talk to
    /// `127.0.0.1:0`-bound mock servers. Production callers
    /// **must not** use this constructor — a script that hits
    /// loopback can reach Redis, the host's own admin endpoints,
    /// cloud metadata services on link-local, etc.
    pub fn with_loopback_allowed_for_tests() -> Self {
        Self {
            allow_loopback: true,
            ..Self::new()
        }
    }

    /// Build a fresh `rhai::Engine` configured with the sandbox
    /// limits + every primitive registered. The returned engine
    /// captures `plugin_id` + a fresh per-plugin cache; reusing
    /// it across plugins would cross the cache.
    ///
    /// Returns `(engine, owning_plugin_slot)` — the slot lets the
    /// caller plant the live `ScriptPlugin` so `ws_subscribe`'s
    /// per-frame callback can invoke handlers on the same engine
    /// without reaching back into the factory.
    pub fn build_for_plugin(&self, plugin_id: &str) -> (rhai::Engine, OwningPluginSlot) {
        let mut engine = rhai::Engine::new();
        engine.set_max_operations(MAX_OPS_PER_CALL);
        engine.set_max_call_levels(MAX_CALL_DEPTH);
        engine.set_max_expr_depths(MAX_EXPR_DEPTH, MAX_EXPR_DEPTH);
        engine.set_max_string_size(MAX_STRING_LEN);
        engine.set_max_array_size(MAX_ARRAY_LEN);
        engine.set_max_map_size(MAX_MAP_LEN);
        // Defense in depth — the host doesn't expose `eval` /
        // `import` / file I/O, but explicit deny is cheap.
        engine.disable_symbol("eval");
        let cache = Arc::new(HttpCache::new());
        let slot = primitives::register(
            &mut engine,
            plugin_id,
            self.http_agent.clone(),
            cache,
            self.allow_loopback,
            self.host_caps.clone(),
        );
        (engine, slot)
    }
}

impl Default for ScriptEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_for_plugin_returns_isolated_engine_per_plugin() {
        let factory = ScriptEngine::new();
        let _e1 = factory.build_for_plugin("a");
        let _e2 = factory.build_for_plugin("b");
        // No assertion beyond "this doesn't panic"; the contract is
        // that each engine is independent. Cross-plugin contamination
        // would surface as a primitive registered against the wrong
        // plugin_id, which the primitives' tests cover.
    }

    #[test]
    fn engine_rejects_runaway_loop_via_operations_limit() {
        let factory = ScriptEngine::new();
        let (engine, _slot) = factory.build_for_plugin("loop");
        let runaway = "let n = 0; loop { n += 1; }";
        let err = engine.eval::<rhai::Dynamic>(runaway).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("Operations") || s.contains("operations"),
            "expected operations-limit error; got: {s}",
        );
    }
}
