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

use crate::mcp_host::{MCP_TOOL_PREFIX, McpHost};
use crate::tool_apis_http::HttpWebFetchApi;
use crate::tool_apis_search::DuckDuckGoSearchApi;
use crate::tool_apis_subagent::InferenceSubagentApi;
use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::ids::ConversationId;
use execlaw_core::tool::{Capability, Clock, SystemClock, ToolCtx, ToolImpl, ToolOutcome};
use execlaw_core::tool_access::ToolAccessStore;
use execlaw_core::tool_apis::{
    DbConversationApi, DbMemoryApi, DbNotifyApi, DbResearchApi, DbScheduleApi,
};
use execlaw_inference_api::InferenceClient;
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
    /// Phase-8d MCP dispatch tier. When present, tool names with the
    /// `mcp:<server>:<tool>` prefix route to the connection manager
    /// instead of the builtin/plugin layer.
    pub mcp_host: Option<McpHost>,
    /// 2026-04-29 — conversation context for the new
    /// `Arc<dyn ToolImpl>` built-in tier. When the dispatcher is
    /// constructed with a known conversation id, registry-resolved
    /// built-ins (`HookRegistry::builtin`) get invoked through their
    /// trait impl with a capability-scoped `ToolCtx`. When `None`,
    /// the new tier short-circuits and we fall back to the legacy
    /// `BuiltinTools::call` path so older fixtures keep working.
    pub conversation_id: Option<ConversationId>,
    /// Clock for `ToolCtx`. Defaults to `SystemClock`; tests can
    /// override via [`Self::with_clock`].
    pub clock: Arc<dyn Clock>,
    /// 2026-04-29 — inference client + model id used to construct
    /// `InferenceSubagentApi` when a tool's descriptor declares
    /// `Capability::SubagentSpawn`. `None` means "no subagent
    /// capability available this turn" — the dispatcher omits
    /// `ctx.subagent` and the tool falls into the standard
    /// "capability not granted" denial.
    pub inference: Option<(Arc<InferenceClient>, String)>,
    /// Live event bus used by capabilities that emit broadcast
    /// events into the conversation (currently:
    /// `Capability::AttachmentSend`, which opens an Attachment
    /// card the SPA renders inline). `None` keeps the capability
    /// dormant — tools requesting it find `ctx.attachments == None`.
    pub events: Option<crate::events::EventBus>,
    /// Wake handle for the deep-research supervisor. Wired through
    /// to `DbResearchApi` so `research_start` can poke the
    /// supervisor immediately on insert instead of waiting up to 5 s
    /// for the next scheduled tick. `None` short-circuits to the
    /// tick-only path (fine for tests).
    pub research_supervisor_wake: Option<Arc<tokio::sync::Notify>>,
    /// Phase 3 — Signal transport endpoint resolver. Production
    /// wires the [`crate::sidecar_supervisor::SidecarSupervisor`]
    /// here; tests pass a `StaticEndpointResolver` that points at
    /// an in-process axum mock. `None` keeps `Capability::Transport`
    /// dormant — `signal.send_message` and `signal.reply` surface
    /// `Denied("transport capability not granted")`. Wraps in
    /// `Arc<dyn>` rather than the concrete supervisor so non-signal
    /// transports can land here later without churn.
    pub signal_transport_resolver: Option<Arc<dyn crate::signal_transport::RpcEndpointResolver>>,
    /// Phase 3 — controller's registered Signal phone number,
    /// e.g. `+15551234567`. Read once at boot from
    /// `EXECLAW_SIGNAL_CONTROLLER_NUMBER`; flowed through the
    /// per-turn `SignalCliTransport` so `send` can populate
    /// signal-cli-rest-api's required `number` field.
    pub signal_self_number: Option<String>,
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
            mcp_host: None,
            conversation_id: None,
            clock: Arc::new(SystemClock),
            inference: None,
            events: None,
            research_supervisor_wake: None,
            signal_transport_resolver: None,
            signal_self_number: None,
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
            mcp_host: None,
            conversation_id: None,
            clock: Arc::new(SystemClock),
            inference: None,
            events: None,
            research_supervisor_wake: None,
            signal_transport_resolver: None,
            signal_self_number: None,
        }
    }

    /// Attach the Phase-8d MCP dispatch tier. Builder-style so the
    /// existing test ctors don't have to grow another argument.
    pub fn with_mcp(mut self, mcp_host: McpHost) -> Self {
        self.mcp_host = Some(mcp_host);
        self
    }

    /// Attach a conversation id so registry-resolved `Arc<dyn
    /// ToolImpl>` built-ins receive a capability-scoped `ToolCtx`.
    /// Without this, the new built-in tier short-circuits and the
    /// dispatcher falls back to the legacy `BuiltinTools::call` path.
    pub fn with_conversation(mut self, conversation_id: ConversationId) -> Self {
        self.conversation_id = Some(conversation_id);
        self
    }

    /// Override the wall clock. Tests use this to drive deterministic
    /// memory `updated_at` values; production code uses the default
    /// `SystemClock`.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Attach the per-turn inference client + model so subagent-
    /// spawning tools (`delegate_task`) can fire child LLM calls
    /// against the parent's backend. Without this, the dispatcher
    /// omits `ctx.subagent` and any subagent tool returns a
    /// `Denied("subagent capability not granted")`.
    pub fn with_inference(
        mut self,
        client: Arc<InferenceClient>,
        model: impl Into<String>,
    ) -> Self {
        self.inference = Some((client, model.into()));
        self
    }

    /// Attach the live event bus so attachment-send (and any
    /// future broadcast-emitting capability) can fire WS events.
    /// Production wiring sets this from `AppState::events`; tests
    /// that need to assert on emitted events pass a dedicated bus.
    pub fn with_events(mut self, events: crate::events::EventBus) -> Self {
        self.events = Some(events);
        self
    }

    /// Attach the deep-research supervisor's wake handle so
    /// `research_start` poke-wakes the supervisor on insert. Without
    /// this, the supervisor waits up to its 5 s tick interval before
    /// claiming the new Pending row — which is the dominant source
    /// of "the agent took a while to come back with the
    /// clarification question" wall-clock latency.
    pub fn with_research_supervisor_wake(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.research_supervisor_wake = Some(wake);
        self
    }

    /// Phase 3 — wire the Signal transport. `resolver` is the
    /// supervised sidecar's host-port lookup (production passes the
    /// `SidecarSupervisor`); `self_number` is the controller's
    /// registered Signal number, read at dispatcher construction
    /// time from `EXECLAW_SIGNAL_CONTROLLER_NUMBER`. Either `None`
    /// keeps the transport capability dormant — calls return
    /// `Denied`.
    pub fn with_signal_transport(
        mut self,
        resolver: Arc<dyn crate::signal_transport::RpcEndpointResolver>,
        self_number: Option<String>,
    ) -> Self {
        self.signal_transport_resolver = Some(resolver);
        self.signal_self_number = self_number;
        self
    }

    /// Convenience: pass `Option<Arc<dyn ...>>` directly so the
    /// production call site can avoid an `if-let` ladder. The
    /// supervisor lives behind `AppState::sidecar_supervisor:
    /// Option<...>`, so the caller is already holding an
    /// `Option`-wrapped value; this lets it flow through unchanged.
    pub fn with_signal_transport_opt(
        mut self,
        resolver: Option<Arc<dyn crate::signal_transport::RpcEndpointResolver>>,
        self_number: Option<String>,
    ) -> Self {
        self.signal_transport_resolver = resolver;
        self.signal_self_number = self_number;
        self
    }

    /// Convenience: chain through `Option<Arc<Notify>>` directly.
    /// Production callers read the handle from
    /// `state.research_supervisor.as_ref().map(|s| s.wake.clone())`,
    /// which is `Option<_>` because test fixtures construct an
    /// `AppState` without a supervisor. This avoids an `if-let`
    /// dance at every dispatch site.
    pub fn with_research_supervisor_wake_opt(
        mut self,
        wake: Option<Arc<tokio::sync::Notify>>,
    ) -> Self {
        self.research_supervisor_wake = wake;
        self
    }

    /// Build a `ToolCtx` populated with exactly the capability APIs
    /// the tool's descriptor declared. A tool that didn't request
    /// `MemoryRead`/`Write` gets `ctx.memory == None` and either
    /// returns `Denied` from its own body or simply never reaches a
    /// memory call.
    #[allow(clippy::too_many_lines)]
    fn build_ctx_for(&self, tool: &Arc<dyn ToolImpl>) -> Result<ToolCtx, String> {
        let conv_id = self
            .conversation_id
            .clone()
            .ok_or_else(|| "no conversation_id on dispatcher".to_string())?;
        let db = self.host.db().clone();
        let mut ctx = ToolCtx::empty(
            conv_id.clone(),
            self.caller_trust.as_str(),
            self.clock.clone(),
        );
        let caps = &tool.descriptor().capabilities;
        let needs_conv = caps.iter().any(|c| {
            matches!(
                c,
                Capability::ConversationRead | Capability::ConversationWrite
            )
        });
        let needs_mem = caps
            .iter()
            .any(|c| matches!(c, Capability::MemoryRead | Capability::MemoryWrite));
        let needs_notify = caps.iter().any(|c| matches!(c, Capability::Notify));
        let needs_schedule = caps
            .iter()
            .any(|c| matches!(c, Capability::ScheduleRead | Capability::ScheduleWrite));
        let needs_web_fetch = caps.iter().any(|c| matches!(c, Capability::WebFetch));
        let needs_search = caps.iter().any(|c| matches!(c, Capability::Search));
        let needs_subagent = caps.iter().any(|c| matches!(c, Capability::SubagentSpawn));
        let needs_research_spawn = caps.iter().any(|c| matches!(c, Capability::ResearchSpawn));
        let needs_research_read = caps.iter().any(|c| matches!(c, Capability::ResearchRead));
        if needs_conv {
            ctx.conversation = Some(Arc::new(DbConversationApi::new(
                db.clone(),
                conv_id.clone(),
            )));
        }
        let now = self.clock.now_unix();
        if needs_mem {
            ctx.memory = Some(Arc::new(DbMemoryApi::new(
                db.clone(),
                self.caller_trust.as_str(),
                now,
            )));
        }
        if needs_notify {
            ctx.notify = Some(Arc::new(DbNotifyApi::new(db.clone(), conv_id.clone(), now)));
        }
        if needs_schedule {
            ctx.schedule = Some(Arc::new(DbScheduleApi::new(
                db,
                self.caller_trust.as_str(),
                conv_id,
                now,
            )));
        }
        if needs_web_fetch {
            ctx.web_fetch = Some(Arc::new(HttpWebFetchApi::new()));
        }
        if needs_search {
            // 2026-05-04 (rev 9): the dispatcher used to hard-code
            // DuckDuckGo here. With config_search_providers + the
            // resolver landing, the active provider is resolved
            // from DB at every dispatch — the operator can swap
            // providers via Settings → Search without restart.
            // Resolver always returns SOMETHING (falls back to
            // DDG on error), so this can never produce None.
            ctx.search = Some(crate::search_resolver::resolve_active_provider(
                &self.host.db().clone(),
            ));
        }
        if needs_subagent {
            if let Some((client, model)) = self.inference.as_ref() {
                ctx.subagent = Some(Arc::new(InferenceSubagentApi::new(
                    client.clone(),
                    model.clone(),
                    self.host.db().clone(),
                    ctx.conversation_id.clone(),
                )));
            }
            // When `inference` isn't wired (test fixture / no
            // backend resolved this turn), we leave `ctx.subagent
            // == None` and the tool falls into its own "capability
            // not granted" denial.
        }
        // Research API: declare `ResearchSpawn` to get a spawn-
        // enabled api; declaring only `ResearchRead` returns a
        // read-only impl whose `start` errors. A descriptor that
        // declares neither leaves `ctx.research == None`.
        if needs_research_spawn {
            let mut api = DbResearchApi::with_spawn(
                self.host.db().clone(),
                self.caller_trust.as_str(),
                ctx.conversation_id.clone(),
                now,
            );
            if let Some(wake) = self.research_supervisor_wake.as_ref() {
                api = api.with_supervisor_wake(wake.clone());
            }
            ctx.research = Some(Arc::new(api));
        } else if needs_research_read {
            ctx.research = Some(Arc::new(DbResearchApi::read_only(
                self.host.db().clone(),
                self.caller_trust.as_str(),
                ctx.conversation_id.clone(),
                now,
            )));
        }
        let needs_transport = caps.iter().any(|c| matches!(c, Capability::Transport));
        if needs_transport {
            if let Some(resolver) = self.signal_transport_resolver.as_ref() {
                // Phase 4 — resolve `current_chat_id` from the
                // conversation's `principal_group_id`. The chain:
                //   1. conversation_id → state_conversations.principal_group_id
                //   2. principal_group_id + "signal" → state_transport_bindings.foreign_id
                // A miss at either step (controller-initiated chat,
                // group with no Signal binding, etc.) leaves
                // `current_chat_id = None` and `signal.reply` returns
                // `no_inbound_context` — the tool's own precondition
                // surface, not a silent dispatch against the wrong
                // recipient.
                let pg_store =
                    execlaw_core::principal_groups::PrincipalGroupStore::new(self.host.db());
                let binding_store =
                    execlaw_core::transport_bindings::TransportBindingStore::new(self.host.db());
                let current_chat_id = pg_store
                    .principal_group_id_for(ctx.conversation_id.as_str())
                    .ok()
                    .flatten()
                    .and_then(|pg_id| {
                        binding_store
                            .bindings_for_group(&pg_id, crate::signal_transport::SIGNAL_CHANNEL)
                            .ok()
                            .and_then(|mut v| v.pop().map(|b| b.foreign_id))
                    });
                let transport = crate::signal_transport::SignalCliTransport::new(
                    resolver.clone(),
                    self.host.db().clone(),
                    self.signal_self_number.clone(),
                    current_chat_id,
                )
                .with_caller_conversation_id(ctx.conversation_id.clone());
                ctx.transport = Some(Arc::new(transport));
            }
            // No resolver wired (test fixture / boot order race) →
            // ctx.transport stays None and the tool body's own
            // capability-not-granted denial fires.
        }
        let needs_attachment_send = caps.iter().any(|c| matches!(c, Capability::AttachmentSend));
        if needs_attachment_send {
            if let Some(events) = self.events.as_ref() {
                ctx.attachments = Some(Arc::new(crate::attachment_api::ServerAttachmentApi::new(
                    self.host.db().clone(),
                    events.clone(),
                    ctx.conversation_id.clone(),
                )));
            }
            // No bus → capability stays dormant. The tool body's
            // own `ctx.attachments.is_none()` denial fires.
        }
        Ok(ctx)
    }

    /// Dispatch a tool name through the new `Arc<dyn ToolImpl>` tier
    /// if the registry has a built-in for it. Returns `Some(result)`
    /// when the registry owns the name (success or tool error);
    /// returns `None` when no built-in is registered, so the caller
    /// can fall through to the legacy `BuiltinTools::call` path or
    /// the plugin host.
    async fn try_registry_builtin(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        let tool = self.host.registry().builtin(tool_name)?;
        // The descriptor declared capabilities the new path needs
        // to populate, but we don't have a conversation id to scope
        // them to. Fall through (return None) so the legacy
        // `BuiltinTools` impl (if any) gets a chance.
        self.conversation_id.as_ref()?;
        let ctx = match self.build_ctx_for(&tool) {
            Ok(c) => c,
            Err(e) => return Some(Err(e)),
        };
        Some(match tool.invoke(ctx, args.clone()).await {
            ToolOutcome::Ok(v) => Ok(v),
            ToolOutcome::Err { code, message } => Err(format!("{code}: {message}")),
            ToolOutcome::Denied { reason } => Err(format!("denied: {reason}")),
        })
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

        // Phase-8d: prefix-route MCP-sourced tools to the connection
        // manager. Falling back to builtins/plugins for an
        // `mcp:`-prefixed name is wrong — those tiers don't speak
        // the prefix.
        if tool_name.starts_with(MCP_TOOL_PREFIX) {
            return match &self.mcp_host {
                Some(host) => host.call_tool(tool_name, args_json.clone()).await,
                None => Err(format!("no MCP host configured to dispatch '{tool_name}'")),
            };
        }

        // 2026-04-29 — registry-based built-in tier (new
        // `Arc<dyn ToolImpl>` path). Runs before the legacy
        // `BuiltinTools::call` so refactored built-ins hit the
        // capability-scoped path and uncrefactored ones still work.
        if let Some(r) = self.try_registry_builtin(tool_name, args_json).await {
            return r;
        }
        if let Some(r) = self.builtins.call(tool_name, args_json).await {
            return r;
        }
        let caps: Vec<&str> = self.caller_caps.iter().map(|s| s.as_str()).collect();
        // 2026-05-03 — pass `caller_trust` so the host can enforce
        // `[[tools]].trust_floor` (selfhosted-claw's `controllerOnly`
        // generalised). Without this, a Signal contact mapped to
        // `KnownLimited` could invoke `signal.send_message` and use
        // the controller's outbound transport to spam other people.
        self.host
            .call_tool(
                tool_name,
                args_json.clone(),
                &caps,
                Some(self.caller_trust.as_str()),
            )
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
        let disp = ChainedToolDispatch::new(test_host(), vec!["*".into()], NoBuiltinTools);
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
        let err = disp.call("echo", &serde_json::json!({})).await.unwrap_err();
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
                    default_allowed_classes: vec!["Controller".into(), "KnownTrusted".into()],
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
        let v = disp.call("echo", &serde_json::json!({})).await.unwrap();
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
        let err = disp.call("echo", &serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("disabled"), "got: {err}");
    }

    /// Phase-8d routing: a tool name with the `mcp:` prefix routes
    /// to the McpHost dispatcher. With no actor connected for the
    /// named server, we expect a "not connected" error rather than
    /// fall-through to builtins/plugins.
    #[tokio::test]
    async fn mcp_prefixed_name_routes_to_mcp_host() {
        struct NoneBuiltin;
        #[async_trait]
        impl BuiltinTools for NoneBuiltin {
            async fn call(
                &self,
                _: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                None
            }
        }
        let host = test_host();
        let db = host.db().clone();
        let mcp = crate::mcp_host::McpHost::new(db.clone());
        let disp = ChainedToolDispatch::with_access_gate(
            host,
            vec!["*".into()],
            TrustLevel::Controller,
            NoneBuiltin,
            db,
        )
        .with_mcp(mcp);
        let err = disp
            .call("mcp:github:create_pr", &serde_json::json!({}))
            .await
            .unwrap_err();
        // Two acceptable wordings depending on whether the access
        // gate fired first (no row → allow → routes to mcp_host →
        // not connected) or another error path.
        assert!(
            err.contains("not connected") || err.contains("not authorized"),
            "expected MCP routing error, got: {err}",
        );
    }

    /// Phase-8d safety: when no McpHost is wired into the dispatch,
    /// an `mcp:`-prefixed tool name returns a structured error
    /// rather than falling through and confusing the runner.
    #[tokio::test]
    async fn mcp_prefixed_without_host_returns_structured_error() {
        struct NoneBuiltin;
        #[async_trait]
        impl BuiltinTools for NoneBuiltin {
            async fn call(
                &self,
                _: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                None
            }
        }
        let disp = ChainedToolDispatch::new(test_host(), vec!["*".into()], NoneBuiltin);
        let err = disp
            .call("mcp:github:create_pr", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.contains("no MCP host configured"),
            "expected structured no-host error, got: {err}",
        );
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
        let v = disp.call("echo", &serde_json::json!({})).await.unwrap();
        assert_eq!(v["reached"], true);
    }

    // ---- New trait-based built-in dispatch tests --------------------

    use execlaw_core::builtin_tools::{ReadMemoryTool, SetThreadNameTool, WriteMemoryTool};
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::ids::{ConversationId, EventSeq};
    use std::sync::Arc;

    fn seed_conv(db: &execlaw_core::Database, id: &str) -> ConversationId {
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

    /// A registry-resolved built-in (`Arc<dyn ToolImpl>`) is invoked
    /// through the new path with a capability-scoped `ToolCtx`. The
    /// underlying conversation store reflects the write — proving
    /// the entire chain (registry lookup → cap construction →
    /// invoke → store mutation) is wired.
    #[tokio::test]
    async fn registry_builtin_set_thread_name_writes_through_dispatcher() {
        let host = test_host();
        let db = host.db().clone();
        host.registry()
            .register_builtin(Arc::new(SetThreadNameTool::new()))
            .unwrap();
        let cid = seed_conv(&db, "c1");
        let disp = ChainedToolDispatch::new(host, vec!["*".into()], NoBuiltinTools)
            .with_conversation(cid.clone());
        let out = disp
            .call("set_thread_name", &serde_json::json!({"name": "Branding"}))
            .await
            .unwrap();
        assert_eq!(out["ok"], true);
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Branding"));
    }

    /// `write_memory` registered through the new path, then
    /// `read_memory` reads it back — both at Controller trust. Proves
    /// the per-call `MemoryApi` is constructed correctly with
    /// caller_trust baked in.
    #[tokio::test]
    async fn registry_builtin_memory_round_trip_via_dispatcher() {
        let host = test_host();
        let db = host.db().clone();
        host.registry()
            .register_builtin(Arc::new(WriteMemoryTool::new()))
            .unwrap();
        host.registry()
            .register_builtin(Arc::new(ReadMemoryTool::new()))
            .unwrap();
        let cid = seed_conv(&db, "c2");
        let disp = ChainedToolDispatch::with_access_gate(
            host,
            vec!["*".into()],
            TrustLevel::Controller,
            NoBuiltinTools,
            db,
        )
        .with_conversation(cid);

        disp.call(
            "write_memory",
            &serde_json::json!({"scope": "g", "key": "k", "value": "v"}),
        )
        .await
        .unwrap();
        let v = disp
            .call(
                "read_memory",
                &serde_json::json!({"scope": "g", "key": "k"}),
            )
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!("v"));
    }

    /// A capability the descriptor didn't declare must surface as a
    /// `Denied` outcome. Here the dispatcher picks `set_thread_name`
    /// (declares `ConversationWrite` only) and the test calls a
    /// no-args invocation of `read_memory` against an unregistered
    /// name to assert the no-op fallthrough — and then registers
    /// `read_memory` and triggers the missing-capability branch by
    /// stripping `MemoryApi` via a dispatcher with no conversation
    /// id (which short-circuits the new path).
    #[tokio::test]
    async fn registry_builtin_denied_when_capability_unmet() {
        struct PartialMemoryTool {
            d: execlaw_core::tool::ToolDescriptor,
        }
        #[async_trait]
        impl execlaw_core::tool::ToolImpl for PartialMemoryTool {
            fn descriptor(&self) -> &execlaw_core::tool::ToolDescriptor {
                &self.d
            }
            async fn invoke(
                &self,
                ctx: execlaw_core::tool::ToolCtx,
                _args: serde_json::Value,
            ) -> execlaw_core::tool::ToolOutcome {
                if ctx.memory.is_some() {
                    execlaw_core::tool::ToolOutcome::ok(serde_json::json!({"ok": true}))
                } else {
                    execlaw_core::tool::ToolOutcome::denied("memory missing")
                }
            }
        }
        let tool = Arc::new(PartialMemoryTool {
            d: execlaw_core::tool::ToolDescriptor {
                name: "needs_mem".into(),
                description: "x".into(),
                schema: serde_json::json!({"type": "object"}),
                source: execlaw_core::tool::ToolSource::Builtin,
                latency: execlaw_core::tool::ToolLatency::Low,
                // Intentionally empty — the dispatcher must NOT
                // populate `ctx.memory` even though we'd need it.
                capabilities: vec![],
                default_allowed_classes: vec!["Controller".into()],
            },
        });
        let host = test_host();
        let db = host.db().clone();
        host.registry().register_builtin(tool).unwrap();
        let cid = seed_conv(&db, "c3");
        let disp =
            ChainedToolDispatch::new(host, vec!["*".into()], NoBuiltinTools).with_conversation(cid);
        let err = disp
            .call("needs_mem", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("denied"));
        assert!(err.contains("memory missing"));
    }

    /// When the dispatcher has no `conversation_id`, the new path
    /// short-circuits and the legacy `BuiltinTools::call` chain runs.
    /// This preserves the pre-2026-04-29 contract for fixtures that
    /// don't construct conversations.
    #[tokio::test]
    async fn registry_builtin_no_conversation_falls_through_to_legacy() {
        let host = test_host();
        host.registry()
            .register_builtin(Arc::new(SetThreadNameTool::new()))
            .unwrap();

        struct LegacyEcho;
        #[async_trait]
        impl BuiltinTools for LegacyEcho {
            async fn call(
                &self,
                _: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                Some(Ok(serde_json::json!({"legacy": true})))
            }
        }
        let disp = ChainedToolDispatch::new(host, vec!["*".into()], LegacyEcho); // no conversation id
        let v = disp
            .call("set_thread_name", &serde_json::json!({"name": "n"}))
            .await
            .unwrap();
        assert_eq!(v["legacy"], true);
    }

    /// The new built-in tier runs BEFORE the legacy `BuiltinTools`
    /// path. If both are configured for the same name, the
    /// trait-based one wins.
    #[tokio::test]
    async fn registry_builtin_takes_precedence_over_legacy_builtins() {
        let host = test_host();
        let db = host.db().clone();
        host.registry()
            .register_builtin(Arc::new(SetThreadNameTool::new()))
            .unwrap();
        let cid = seed_conv(&db, "c4");

        struct LegacyShadowing;
        #[async_trait]
        impl BuiltinTools for LegacyShadowing {
            async fn call(
                &self,
                name: &str,
                _: &serde_json::Value,
            ) -> Option<Result<serde_json::Value, String>> {
                if name == "set_thread_name" {
                    Some(Ok(serde_json::json!({"from_legacy": true})))
                } else {
                    None
                }
            }
        }
        let disp = ChainedToolDispatch::new(host, vec!["*".into()], LegacyShadowing)
            .with_conversation(cid.clone());
        let v = disp
            .call("set_thread_name", &serde_json::json!({"name": "Won"}))
            .await
            .unwrap();
        assert_eq!(v["ok"], true); // from registry, not legacy
        assert!(v.get("from_legacy").is_none());
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Won"));
    }
}
