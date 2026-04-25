//! Chains built-in tools + plugin tools into one
//! `execlaw_runner_local::turn::ToolDispatch` that TurnExecutor calls.
//!
//! Lookup order on every tool call:
//! 0. **Per-tool access policy** (Phase 8a): consult
//!    `config_tool_access` for the tool. If the row exists and the
//!    caller's trust class is not in `allowed_classes`, OR the tool
//!    is disabled, OR the source has marked it removed — return
//!    `Err("not authorized: ...")` immediately. This is the single
//!    enforcement point for the operator-driven trust-class allowlist
//!    that applies regardless of whether the tool came from a
//!    built-in, a plugin, or an MCP server.
//! 1. Built-in tools (via [`BuiltinTools`]) — e.g. the memory shim.
//! 2. Plugin-contributed tools (via [`PluginHost::call_tool`]) — with
//!    capability enforcement: the caller's `capability_set` must be a
//!    superset of the tool's `required_capabilities`, or the dispatch
//!    fails BEFORE the subprocess sees the args (§7.2 + §7.3).
//! 3. Anything else → `Err("no tool registered for '<name>'")`. That
//!    error is paired with a cancellation `tool_result` by
//!    `commit_turn`'s enforce_tool_pairing, so the log stays
//!    well-formed even when the model hallucinates a tool name.

use async_trait::async_trait;
use execlaw_core::tool_access::ToolAccessStore;
use execlaw_core::Database;
use execlaw_plugin_host::{BuiltinTools, PluginHost};
use execlaw_policy::trust::TrustLevel;
use execlaw_runner_local::turn::ToolDispatch;
use std::sync::Arc;

/// Concrete dispatcher built from a `PluginHost` + built-ins +
/// caller capability set + caller trust class (the access gate).
pub struct ChainedToolDispatch<B: BuiltinTools> {
    pub host: PluginHost,
    pub caller_caps: Vec<String>,
    pub caller_trust: TrustLevel,
    pub builtins: B,
    /// Database handle the dispatch consults for `config_tool_access`
    /// rows. Optional so test fixtures and pre-Phase-8a callers can
    /// keep working without seeding the gate; `None` means "skip the
    /// trust-class allowlist check," which is the legacy behaviour.
    pub access_db: Option<Database>,
}

impl<B: BuiltinTools> ChainedToolDispatch<B> {
    /// Legacy ctor — kept so existing call sites compile. `caller_trust`
    /// defaults to `Controller` and the access gate is disabled, which
    /// matches the pre-Phase-8a "no tool gate" semantic.
    pub fn new(host: PluginHost, caller_caps: Vec<String>, builtins: B) -> Self {
        Self {
            host,
            caller_caps,
            caller_trust: TrustLevel::Controller,
            builtins,
            access_db: None,
        }
    }

    /// Phase-8a ctor: wire the caller's trust class + the
    /// `config_tool_access` store. Production code paths use this so
    /// the per-tool allowlist is enforced on every dispatch.
    pub fn with_access_gate(
        host: PluginHost,
        caller_caps: Vec<String>,
        caller_trust: TrustLevel,
        builtins: B,
        access_db: Database,
    ) -> Self {
        Self {
            host,
            caller_caps,
            caller_trust,
            builtins,
            access_db: Some(access_db),
        }
    }

    pub fn into_arc(self) -> Arc<dyn ToolDispatch>
    where
        B: 'static,
    {
        Arc::new(self)
    }

    /// Per-tool access check. Returns `Ok(())` when the call should
    /// proceed, `Err(reason)` when it must be denied. Centralises the
    /// rules so the dispatch chain has exactly one enforcement point.
    fn check_access(&self, tool_name: &str) -> Result<(), String> {
        let Some(db) = &self.access_db else {
            return Ok(()); // gate disabled (test / legacy)
        };
        let row = ToolAccessStore::new(db)
            .get(tool_name)
            .map_err(|e| format!("tool_access lookup failed: {e}"))?;
        let Some(row) = row else {
            // No policy row yet — happens transiently between boot
            // and the first sync, or for a tool the registry-sync
            // hasn't reflected. Allow so the runner doesn't grind to
            // a halt; production sync runs early enough that this is
            // a brief, harmless window.
            return Ok(());
        };
        if !row.enabled {
            return Err(format!("not authorized: tool '{tool_name}' is disabled"));
        }
        if row.removed_at.is_some() {
            return Err(format!(
                "not authorized: tool '{tool_name}' is no longer registered by its source"
            ));
        }
        let caller = self.caller_trust.as_str();
        if !row.allowed_classes.iter().any(|c| c == caller) {
            return Err(format!(
                "not authorized: tool '{tool_name}' is not allowed for trust class {caller}"
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl<B: BuiltinTools + 'static> ToolDispatch for ChainedToolDispatch<B> {
    async fn call(
        &self,
        tool_name: &str,
        args_json: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Phase-8a access gate runs FIRST so a denied call never
        // reaches a builtin's side-effect, a plugin subprocess, or an
        // MCP server.
        self.check_access(tool_name)?;

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

    /// Phase-8a gate: a tool that has a policy row but the caller's
    /// trust class isn't on the allowlist must be denied BEFORE
    /// builtins or plugins are consulted.
    #[tokio::test]
    async fn access_gate_denies_caller_outside_allowed_classes() {
        use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
        let host = test_host();
        let db = host.db().clone();
        // Seed a policy row that allows ONLY Controller — but the
        // caller is KnownTrusted, so the gate should fire before the
        // builtin even gets a chance.
        ToolAccessStore::new(&db)
            .upsert_seen(
                &ToolAccessSeed {
                    tool_name: "echo".into(),
                    source: ToolSource::Builtin,
                    source_id: None,
                    description: None,
                    input_schema: None,
                    default_allowed_classes: vec!["Controller".into()],
                },
                0,
            )
            .unwrap();

        struct EchoBuiltin;
        #[async_trait]
        impl BuiltinTools for EchoBuiltin {
            async fn call(
                &self,
                name: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                if name == "echo" {
                    Some(Ok(serde_json::json!({"reached": true})))
                } else {
                    None
                }
            }
        }

        let disp = ChainedToolDispatch::with_access_gate(
            host,
            vec!["*".into()],
            TrustLevel::KnownTrusted,
            EchoBuiltin,
            db,
        );
        let err = disp
            .call("echo", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.contains("not authorized") && err.contains("KnownTrusted"),
            "expected denial mentioning trust class, got: {err}",
        );
    }

    /// Phase-8a gate: when the caller IS in `allowed_classes`, the
    /// dispatch proceeds normally and the builtin's side-effect runs.
    #[tokio::test]
    async fn access_gate_allows_caller_in_allowed_classes() {
        use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
        let host = test_host();
        let db = host.db().clone();
        ToolAccessStore::new(&db)
            .upsert_seen(
                &ToolAccessSeed {
                    tool_name: "echo".into(),
                    source: ToolSource::Builtin,
                    source_id: None,
                    description: None,
                    input_schema: None,
                    default_allowed_classes: vec![
                        "Controller".into(),
                        "KnownTrusted".into(),
                    ],
                },
                0,
            )
            .unwrap();

        struct EchoBuiltin;
        #[async_trait]
        impl BuiltinTools for EchoBuiltin {
            async fn call(
                &self,
                name: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                if name == "echo" {
                    Some(Ok(serde_json::json!({"reached": true})))
                } else {
                    None
                }
            }
        }

        let disp = ChainedToolDispatch::with_access_gate(
            host,
            vec!["*".into()],
            TrustLevel::KnownTrusted,
            EchoBuiltin,
            db,
        );
        let v = disp
            .call("echo", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["reached"], true);
    }

    /// Phase-8a gate: a tool flipped `enabled = false` is denied
    /// regardless of trust class.
    #[tokio::test]
    async fn access_gate_denies_disabled_tool() {
        use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
        let host = test_host();
        let db = host.db().clone();
        let store = ToolAccessStore::new(&db);
        store
            .upsert_seen(
                &ToolAccessSeed {
                    tool_name: "echo".into(),
                    source: ToolSource::Builtin,
                    source_id: None,
                    description: None,
                    input_schema: None,
                    default_allowed_classes: vec!["Controller".into()],
                },
                0,
            )
            .unwrap();
        store
            .set_policy("echo", false, &["Controller".into()])
            .unwrap();

        struct EchoBuiltin;
        #[async_trait]
        impl BuiltinTools for EchoBuiltin {
            async fn call(
                &self,
                _: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                Some(Ok(serde_json::json!({"reached": true})))
            }
        }
        let disp = ChainedToolDispatch::with_access_gate(
            host,
            vec!["*".into()],
            TrustLevel::Controller,
            EchoBuiltin,
            db,
        );
        let err = disp
            .call("echo", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("disabled"), "got: {err}");
    }

    /// Phase-8a gate: with NO policy row at all (the test default),
    /// the legacy "allow" path is preserved so existing tests don't
    /// need rewrites.
    #[tokio::test]
    async fn access_gate_falls_back_to_allow_when_no_row_exists() {
        struct EchoBuiltin;
        #[async_trait]
        impl BuiltinTools for EchoBuiltin {
            async fn call(
                &self,
                _: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                Some(Ok(serde_json::json!({"reached": true})))
            }
        }
        let host = test_host();
        let db = host.db().clone();
        let disp = ChainedToolDispatch::with_access_gate(
            host,
            vec!["*".into()],
            TrustLevel::Blocked, // even Blocked allowed when no row exists
            EchoBuiltin,
            db,
        );
        let v = disp
            .call("echo", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(v["reached"], true);
    }
}
