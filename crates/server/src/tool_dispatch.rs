//! Chains built-in tools + plugin tools into one
//! `execlaw_runner_local::turn::ToolDispatch` that TurnExecutor calls.
//!
//! Lookup order on every tool call:
//! 1. Built-in tools (via [`BuiltinTools`]) — e.g. the memory shim.
//! 2. Plugin-contributed tools (via [`PluginHost::call_tool`]) — with
//!    capability enforcement: the caller's `capability_set` must be a
//!    superset of the tool's `required_capabilities`, or the dispatch
//!    fails BEFORE the subprocess sees the args (§7.2 + §7.3).
//! 3. Anything else → `Err("no tool registered for '<name>'")`. That
//!    error is paired with a cancellation `tool_result` by
//!    `commit_turn`'s enforce_tool_pairing, so the log stays
//!    well-formed even when the model hallucinates a tool name.
//!
//! This module is the integration point between Phase 1 (TurnExecutor)
//! and Phase 2 (PluginHost) — wiring it into `chats::run_real_turn` is
//! the next iteration's job once the non-streaming tool path is
//! reinstated.

use async_trait::async_trait;
use execlaw_plugin_host::{BuiltinTools, PluginHost};
use execlaw_runner_local::turn::ToolDispatch;
use std::sync::Arc;

/// Concrete dispatcher built from a `PluginHost` + built-ins +
/// caller capability set (minted by the policy engine before the turn).
pub struct ChainedToolDispatch<B: BuiltinTools> {
    pub host: PluginHost,
    pub caller_caps: Vec<String>,
    pub builtins: B,
}

impl<B: BuiltinTools> ChainedToolDispatch<B> {
    pub fn new(host: PluginHost, caller_caps: Vec<String>, builtins: B) -> Self {
        Self {
            host,
            caller_caps,
            builtins,
        }
    }

    pub fn into_arc(self) -> Arc<dyn ToolDispatch>
    where
        B: 'static,
    {
        Arc::new(self)
    }
}

#[async_trait]
impl<B: BuiltinTools + 'static> ToolDispatch for ChainedToolDispatch<B> {
    async fn call(
        &self,
        tool_name: &str,
        args_json: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if let Some(r) = self.builtins.call(tool_name, args_json).await {
            return r;
        }
        let caps: Vec<&str> = self.caller_caps.iter().map(|s| s.as_str()).collect();
        self.host
            .call_tool(tool_name, args_json.clone(), &caps)
            .await
    }
}

/// An empty built-ins set — useful when the server is serving turns
/// with no runner-local tools at all (Phase 2 dev path when the memory
/// shim hasn't been wired through yet).
pub struct NoBuiltinTools;

#[async_trait]
impl BuiltinTools for NoBuiltinTools {
    async fn call(
        &self,
        _tool_name: &str,
        _args: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_plugin_host::HookRegistry;

    fn test_host() -> PluginHost {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Leak: tests that need cleanup manage their own TempDir. For
        // the in-memory cases below the dir is never written to.
        let path = dir.keep();
        PluginHost::new(db, HookRegistry::new(), path)
    }

    /// Built-ins take precedence over plugins — if a built-in handles
    /// the call, the plugin registry isn't even consulted.
    #[tokio::test]
    async fn builtin_takes_precedence_over_plugin() {
        struct EchoBuiltin;
        #[async_trait]
        impl BuiltinTools for EchoBuiltin {
            async fn call(
                &self,
                name: &str,
                args: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                if name == "echo" {
                    Some(Ok(serde_json::json!({"builtin": args})))
                } else {
                    None
                }
            }
        }
        let disp = ChainedToolDispatch::new(test_host(), vec!["*".into()], EchoBuiltin);
        let got = disp
            .call("echo", &serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(got["builtin"]["x"], 1);
    }

    /// Unknown tool falls through both layers and returns an error
    /// the TurnExecutor pairs with a cancellation tool_result.
    #[tokio::test]
    async fn unknown_tool_produces_err_not_panic() {
        let disp =
            ChainedToolDispatch::new(test_host(), vec!["*".into()], NoBuiltinTools);
        let err = disp
            .call("nonexistent", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("not registered"));
    }
}
