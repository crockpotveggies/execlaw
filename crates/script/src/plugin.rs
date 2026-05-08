//! [`ScriptPlugin`] — one loaded `.rhai` script, ready to dispatch.
//!
//! The host loads a script at plugin-enable time via
//! [`ScriptPlugin::load`]; the parsed AST is cached so per-call
//! dispatch only runs the script's own top-level functions, not
//! the parser.
//!
//! Two dispatch entry points mirror the subprocess-tier RPC shape:
//!
//!   * [`ScriptPlugin::identity_resolve`] — calls the Rhai
//!     `identity_resolve(transport, handle, oauth)` function and
//!     normalises its return into the `{"match": ...}` shape the
//!     host's `resolve_identity` chain expects.
//!
//!   * [`ScriptPlugin::tool_call`] — calls the Rhai
//!     `tool_call(tool_name, args, oauth)` function and returns
//!     the raw response payload.
//!
//! Both are async wrappers that internally `spawn_blocking` the
//! Rhai interpreter so a long-running script doesn't block a
//! tokio worker.

use crate::engine::ScriptEngine;
use crate::errors::{ScriptError, ScriptResult};
use crate::primitives::{json_to_rhai, rhai_to_json, set_owning_plugin};
use rhai::{AST, Dynamic, Engine, Scope};
use std::path::Path;
use std::sync::Arc;

/// Thread-safe wrapper that owns the per-plugin Rhai engine + AST.
/// Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct ScriptPlugin {
    inner: Arc<ScriptPluginInner>,
}

impl std::fmt::Debug for ScriptPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptPlugin")
            .field("plugin_id", &self.inner.plugin_id)
            .finish_non_exhaustive()
    }
}

struct ScriptPluginInner {
    plugin_id: String,
    engine: Engine,
    ast: AST,
    /// Live WS subscription handles registered by `ws_subscribe*`
    /// bindings. Drained by `shutdown()` so plugin uninstall /
    /// disable / reinstall doesn't leak consumer tasks. The host
    /// calls `shutdown` BEFORE removing the plugin from its
    /// internal map.
    subscription_registry: crate::primitives::SubscriptionRegistry,
}

impl ScriptPlugin {
    /// Parse + compile a script source string (typically the
    /// contents of a `.rhai` file). Returns an error if the
    /// script doesn't parse — the host surfaces that as a clean
    /// install-time failure rather than a runtime crash.
    pub fn from_source(
        plugin_id: &str,
        source: &str,
        engine_factory: &ScriptEngine,
    ) -> ScriptResult<Self> {
        let (engine, owning_slot, subscription_registry) =
            engine_factory.build_for_plugin(plugin_id);
        let ast = engine
            .compile(source)
            .map_err(|e| ScriptError::ParseError(format!("[{plugin_id}] compile: {e}")))?;
        let plugin = Self {
            inner: Arc::new(ScriptPluginInner {
                plugin_id: plugin_id.to_owned(),
                engine,
                ast,
                subscription_registry,
            }),
        };
        // Plant the live plugin handle so the engine's
        // `ws_subscribe` binding can invoke per-frame Rhai
        // handlers via `invoke_async_owned`. Done after the AST
        // compiles so the slot only holds a fully-constructed
        // plugin.
        set_owning_plugin(&owning_slot, plugin.clone());
        Ok(plugin)
    }

    /// Cancel every WS subscription this plugin opened. Called by
    /// the host on uninstall / disable / reinstall so the
    /// previous instance's consumer tasks don't outlive the
    /// engine that spawned them. Without this, a re-installed
    /// transport plugin double-subscribes to its upstream — the
    /// gateway sees `connectionCount > 1`, every inbound is
    /// dispatched twice through the host pipeline, and the
    /// gateway often crashes from the duplicate work.
    ///
    /// Returns the number of subscriptions that were cancelled.
    /// Idempotent: subsequent calls return 0 once the registry is
    /// drained.
    pub fn shutdown(&self) -> usize {
        crate::primitives::cancel_all_subscriptions(&self.inner.subscription_registry)
    }

    /// Load + parse a script from disk. Convenience wrapper for
    /// the common case where the manifest's `runtime.source`
    /// points at a file in the staged plugin dir.
    pub fn from_file(
        plugin_id: &str,
        path: &Path,
        engine_factory: &ScriptEngine,
    ) -> ScriptResult<Self> {
        let source = std::fs::read_to_string(path)?;
        Self::from_source(plugin_id, &source, engine_factory)
    }

    pub fn plugin_id(&self) -> &str {
        &self.inner.plugin_id
    }

    /// Dispatch `identity_resolve`. Wraps the Rhai call in
    /// `spawn_blocking` so the interpreter doesn't pin a tokio
    /// worker. Returns `{"match": ...}` JSON exactly as the
    /// subprocess-tier providers do; the host's `resolve_identity`
    /// chain is tier-agnostic.
    pub async fn identity_resolve(
        &self,
        transport: &str,
        handle: &str,
        oauth_tokens: serde_json::Map<String, serde_json::Value>,
    ) -> ScriptResult<serde_json::Value> {
        self.invoke_async(
            "identity_resolve",
            vec![
                Dynamic::from(rhai::ImmutableString::from(transport)),
                Dynamic::from(rhai::ImmutableString::from(handle)),
                json_to_rhai(&serde_json::Value::Object(oauth_tokens)),
            ],
        )
        .await
    }

    /// Dispatch `tool_call`. Args are passed as a Rhai map; the
    /// script returns a JSON payload.
    pub async fn tool_call(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        oauth_tokens: serde_json::Map<String, serde_json::Value>,
    ) -> ScriptResult<serde_json::Value> {
        self.invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from(tool_name)),
                json_to_rhai(&args),
                json_to_rhai(&serde_json::Value::Object(oauth_tokens)),
            ],
        )
        .await
    }

    /// Generic dispatch for tests / future methods. Calls the
    /// named Rhai top-level function with the given args.
    pub async fn invoke_async(
        &self,
        function: &'static str,
        args: Vec<Dynamic>,
    ) -> ScriptResult<serde_json::Value> {
        let inner = self.inner.clone();
        let result = tokio::task::spawn_blocking(move || -> ScriptResult<Dynamic> {
            // Each call gets a fresh scope so accidental top-level
            // mutations don't leak across calls.
            let mut scope = Scope::new();
            inner
                .engine
                .call_fn::<Dynamic>(&mut scope, &inner.ast, function, args)
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.contains("Function not found") {
                        ScriptError::MissingFunction(function)
                    } else {
                        ScriptError::Runtime(format!("[{}] {function}: {e}", inner.plugin_id,))
                    }
                })
        })
        .await
        .map_err(|e| ScriptError::Runtime(format!("spawn_blocking: {e}")))??;

        rhai_to_json(result).map_err(ScriptError::BadShape)
    }

    /// Optional lifecycle hook — host calls this once after the
    /// plugin is enabled/hydrated. Plugins use it to spawn long-
    /// lived background tasks (a transport plugin's WS inbound
    /// consumer, a webhook poller, etc.) via `ws_subscribe`.
    /// Best-effort: returns `Ok(())` whether the script defines
    /// `on_enable` or not — `MissingFunction` is converted to a
    /// silent success because the convention is opt-in.
    pub async fn call_on_enable(&self) -> ScriptResult<()> {
        // Don't go through invoke_async — it forces a Rhai → JSON
        // conversion of the return value, and on_enable hooks
        // commonly end with `ws_subscribe(...)` whose return is a
        // WsSubscriptionHandle that isn't serializable. We just
        // care that it ran, not what it returned.
        let inner = self.inner.clone();
        let result = tokio::task::spawn_blocking(move || -> ScriptResult<()> {
            let mut scope = rhai::Scope::new();
            match inner.engine.call_fn::<rhai::Dynamic>(
                &mut scope,
                &inner.ast,
                "on_enable",
                Vec::<rhai::Dynamic>::new(),
            ) {
                Ok(_) => Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Function not found") {
                        Err(ScriptError::MissingFunction("on_enable"))
                    } else {
                        Err(ScriptError::Runtime(format!(
                            "[{}] on_enable: {e}",
                            inner.plugin_id,
                        )))
                    }
                }
            }
        })
        .await
        .map_err(|e| ScriptError::Runtime(format!("spawn_blocking: {e}")))?;
        match result {
            Ok(()) => Ok(()),
            Err(ScriptError::MissingFunction(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Same as `invoke_async` but takes the function name as an
    /// owned `String`. Used by the `ws_subscribe` binding which
    /// captures the operator-supplied callback name dynamically;
    /// `invoke_async` requires `&'static str` because Rhai's
    /// `call_fn` does. The duplication is small and the static
    /// path stays the canonical one for compile-time-known
    /// dispatch points (`tool_call`, `identity_resolve`).
    ///
    /// `MissingFunction` carries `&'static str` so a missing
    /// dynamic-name handler surfaces as a `Runtime` error
    /// instead — the owned name is stamped in the message.
    pub async fn invoke_async_owned(
        &self,
        function: String,
        args: Vec<Dynamic>,
    ) -> ScriptResult<serde_json::Value> {
        let inner = self.inner.clone();
        let result = tokio::task::spawn_blocking(move || -> ScriptResult<Dynamic> {
            let mut scope = Scope::new();
            inner
                .engine
                .call_fn::<Dynamic>(&mut scope, &inner.ast, &function, args)
                .map_err(|e| {
                    ScriptError::Runtime(format!(
                        "[{}] {function}: {e}",
                        inner.plugin_id,
                    ))
                })
        })
        .await
        .map_err(|e| ScriptError::Runtime(format!("spawn_blocking: {e}")))??;

        rhai_to_json(result).map_err(ScriptError::BadShape)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_resolve_returns_null_match_when_oauth_missing() {
        // Tiny script that mimics the contract: no token → null.
        let src = r#"
            fn identity_resolve(transport, handle, oauth) {
                if oauth["controller"] == () {
                    return #{ "match": () };
                }
                #{ "match": #{
                    "stable_principal_id": "pri_x",
                    "confidence": 0.9,
                    "trust_hint": "Contact",
                    "display_name": "X",
                    "tags": [],
                    "resolved_by": "test",
                }}
            }
        "#;
        let factory = ScriptEngine::new();
        let plugin = ScriptPlugin::from_source("test", src, &factory).unwrap();
        let r = plugin
            .identity_resolve("email", "x@x", serde_json::Map::new())
            .await
            .unwrap();
        assert!(r["match"].is_null());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_resolve_returns_match_when_token_present() {
        let src = r#"
            fn identity_resolve(transport, handle, oauth) {
                if oauth["controller"] == () {
                    return #{ "match": () };
                }
                #{ "match": #{
                    "stable_principal_id": "pri_alice",
                    "confidence": 0.95,
                    "trust_hint": "Contact",
                    "display_name": "Alice",
                    "tags": ["test"],
                    "resolved_by": "test-plugin",
                }}
            }
        "#;
        let factory = ScriptEngine::new();
        let plugin = ScriptPlugin::from_source("test", src, &factory).unwrap();
        let mut oauth = serde_json::Map::new();
        oauth.insert(
            "controller".into(),
            serde_json::Value::String("ya29".into()),
        );
        let r = plugin
            .identity_resolve("email", "alice@x.com", oauth)
            .await
            .unwrap();
        assert_eq!(r["match"]["stable_principal_id"], "pri_alice");
        assert_eq!(r["match"]["display_name"], "Alice");
        assert_eq!(r["match"]["resolved_by"], "test-plugin");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_function_surfaces_clean_error() {
        let src = r#"
            fn other_function(x) { x }
        "#;
        let factory = ScriptEngine::new();
        let plugin = ScriptPlugin::from_source("noop", src, &factory).unwrap();
        let err = plugin
            .identity_resolve("email", "x", serde_json::Map::new())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ScriptError::MissingFunction("identity_resolve")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_call_passes_args_and_returns_result() {
        let src = r#"
            fn tool_call(name, args, oauth) {
                #{
                    "tool": name,
                    "echoed_args": args,
                    "had_oauth": oauth.len() > 0,
                }
            }
        "#;
        let factory = ScriptEngine::new();
        let plugin = ScriptPlugin::from_source("echo-tool", src, &factory).unwrap();
        let mut oauth = serde_json::Map::new();
        oauth.insert("controller".into(), serde_json::Value::String("tok".into()));
        let r = plugin
            .tool_call("contacts.list", serde_json::json!({"limit": 50}), oauth)
            .await
            .unwrap();
        assert_eq!(r["tool"], "contacts.list");
        assert_eq!(r["echoed_args"]["limit"], 50);
        assert_eq!(r["had_oauth"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parse_failure_surfaces_at_load_time() {
        let src = "this is not valid rhai !!!";
        let factory = ScriptEngine::new();
        let err = ScriptPlugin::from_source("bad", src, &factory).unwrap_err();
        assert!(matches!(err, ScriptError::ParseError(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn from_file_loads_script_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.rhai");
        std::fs::write(
            &path,
            r#"fn identity_resolve(t, h, o) { #{ "match": () } }"#,
        )
        .unwrap();
        let factory = ScriptEngine::new();
        let plugin = ScriptPlugin::from_file("disk", &path, &factory).unwrap();
        let r = plugin
            .identity_resolve("email", "x", serde_json::Map::new())
            .await
            .unwrap();
        assert!(r["match"].is_null());
        assert_eq!(plugin.plugin_id(), "disk");
    }
}
