//! Shared application state.

use crate::events::EventBus;
use execlaw_core::Database;
use execlaw_plugin_host::PluginHost;
use std::sync::Arc;

/// Configuration for the server process.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: std::net::SocketAddr,
    /// Issuer string placed in JWT claims.
    pub jwt_issuer: String,
    pub access_token_ttl_secs: i64,
    pub refresh_token_ttl_secs: i64,
    // Phase 12.E removed `inference_base_url` from the config —
    // the boot-time URL is now read from `EXECLAW_INFERENCE_URL`
    // directly in `cli/main.rs` and threaded into the
    // `InferenceResolver`'s bootstrap. Per-turn URL selection
    // happens via `state.inference.resolve(db, purpose)` reading
    // `config_backends`. The static config no longer carries it.
    /// System prompt sent on every turn. Phase 1 uses a static
    /// string; later phases make this a per-conversation + per-role
    /// composition.
    pub system_prompt: String,
    /// Model id passed in `/v1/chat/completions` requests.
    pub model_id: String,
    /// Hard cap on tool-call rounds per turn (runaway-loop guard).
    pub max_tool_rounds: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:3031".parse().expect("valid default addr"),
            jwt_issuer: "execlaw".to_owned(),
            access_token_ttl_secs: 15 * 60, // 15 minutes, §7.1
            refresh_token_ttl_secs: 7 * 24 * 60 * 60, // 7 days, §7.1
            // 2026-04-28: replaced the one-line stub with a working
            // baseline lifted from selfhosted-claw's `buildSystemPrompt`
            // restraint section. Without these guardrails the 27B
            // local model wandered into infinite "let me think more"
            // monologues and tool-call ping-pong, especially when no
            // tools were wired up. Operators override the *voice*
            // (DisplayName, Tone, etc.) via `config_personality`;
            // these rules are the static base that owns concision +
            // loop-prevention and sits AFTER personality in the
            // assembled prompt so it has the final word on conflict.
            system_prompt: concat!(
                "You are execlaw, a self-hosted assistant. Follow these rules every turn:\n\n",
                "1. Be concise. Default to 1-3 sentence replies. Expand only when the operator asks for detail.\n",
                "2. Do not narrate your reasoning out loud (\"let me think...\", \"first I'll...\"). Just answer.\n",
                "3. Do not repeat yourself. If you've made a point once, do not restate it later in the same reply.\n",
                "4. Prefer answering from your own knowledge UNLESS the question concerns:\n",
                "   * live or current data — weather, news, prices, sports scores, stock quotes, flight status, traffic;\n",
                "   * anything dated to today/this week/this month/right now;\n",
                "   * the operator's own data — calendar, contacts, notes, prior conversations, scheduled routines;\n",
                "   * facts that may have changed since your training cutoff (use the Turn context block's UTC time as ground truth);\n",
                "   For those, call the relevant tool IMMEDIATELY in the same turn. Do not ask permission, do not announce your plan — just call it.\n",
                "5. NEVER promise to do something and then stop. If you write \"I'll look that up\" or \"let me check\", you must call the tool in the same turn before ending. A turn that ends with an unfulfilled promise is a bug.\n",
                "6. If you've called the same tool twice with similar arguments, stop calling tools and summarise what you've learned.\n",
                "7. If a tool keeps returning errors, stop calling it and explain the failure to the operator instead of retrying blindly.\n",
                "8. Never call a tool just to fill space. If there's genuinely nothing useful to do, finish the turn.\n",
                "9. When you're done answering, stop. Do not ask follow-up questions unless they are required to act.",
            ).to_owned(),
            model_id: "QuantTrio/Qwen3.5-27B-AWQ".to_owned(),
            // 2026-05 — bumped 3 → 8 alongside the agent-prompting
            // audit's #5. The old cap was set when plugin tool
            // descriptions were placeholders ("Plugin tool 'X'
            // (latency: Y)"), so the model wasted rounds rummaging
            // around blindly and we wanted to bail it out fast.
            // With real descriptions + schemas (#1) and routing
            // prose (#2) the model's first 1-2 calls are usually
            // correct, but multi-step flows like
            // "search → fetch → summarise" or
            // "list_calendars → list_events → check_availability →
            // create_event" need 4-6 rounds to complete cleanly.
            // Routines that fan out across multiple sub-tasks
            // (e.g. "weather + 4 news categories + Signal send"
            // for a Daily Briefing) realistically hit 10-14 rounds.
            // 16 keeps headroom for retries while staying inside
            // the runner hard ceiling (`RUNNER_MAX_TOOL_ROUNDS`).
            max_tool_rounds: 16,
        }
    }
}

/// App state handed to every route.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub config: Arc<ServerConfig>,
    /// Ed25519 signing key used for JWT + capability tokens.
    pub signer: Arc<crate::auth::JwtSigner>,
    /// In-memory refresh-token store. SQLite-backed replacement lands
    /// before Phase 2 ships.
    pub refresh_store: Arc<crate::auth::RefreshStore>,
    /// Live-event bus fanning events out to WebSocket subscribers.
    pub events: EventBus,
    /// HMAC key used to sign `state_events` rows (§7.8). `None` during
    /// tests + pre-setup; production loads it from the vault on boot.
    pub event_log_hmac_key: Option<Arc<Vec<u8>>>,
    /// Phase 12.E — inference-client resolver. Picks the right URL
    /// per turn from `config_backends`, falling through to a
    /// boot-time bootstrap client when no row covers the requested
    /// purpose. The resolver itself is always present; its
    /// `resolve` method returns `Option<Arc<InferenceClient>>` so
    /// the chat route still falls back to the stub reply when no
    /// URL is available anywhere.
    pub inference: Arc<crate::inference_resolver::InferenceResolver>,
    /// Plugin lifecycle manager + hook registry. Every route that
    /// enumerates tools / transports / UI panels reads from here.
    pub plugin_host: PluginHost,
    /// Phase 7e WebAuthn relying-party + ceremony state. `None` when
    /// the operator hasn't configured an RP origin (the SPA hides the
    /// "Add WebAuthn" affordance and the login route skips the
    /// second-factor branch in that mode).
    pub webauthn: Option<crate::webauthn::SharedWebauthn>,
    /// Phase 8c MCP connection manager. Owns one tokio actor per
    /// configured MCP server; reflects their tools into
    /// `config_tool_access` and dispatches tool calls when the
    /// runner picks an `mcp:<id>:<name>` tool.
    pub mcp_host: crate::mcp_host::McpHost,
    /// Phase 12.C — supervisor for managed inference backends.
    /// `None` when no `ServiceController` is wired (tests, dev
    /// builds without Docker). Routes that depend on it return
    /// 503 in that mode.
    pub backend_supervisor: Option<crate::backend_supervisor::BackendSupervisor>,
    /// Phase 2b — sidecar (companion-container) supervisor.
    /// Owns Signal-cli / WhatsApp / Matrix / ... sidecars declared
    /// by installed plugins via `[services.sidecar]`. `None` when no
    /// `ServiceController` is wired (tests, dev builds without
    /// Docker); the `/api/admin/sidecars` route returns 503 in
    /// that mode.
    pub sidecar_supervisor: Option<crate::sidecar_supervisor::SidecarSupervisor>,
    /// Channel-keyed registry of host-side `TransportApi`
    /// factories. Bridge call sites (chat text reply auto-bridge,
    /// attachment fan-out, research-PDF dispatch) walk a
    /// conversation's bindings and ask the registry for a
    /// transport without naming Signal / WhatsApp / etc.
    /// directly. Empty in test fixtures and managed-mode boot
    /// paths that don't wire transport plugins; bridge sites
    /// gracefully degrade to "no transport bridged this message"
    /// (the web-side surface already shipped).
    pub host_transports: crate::transport_registry::HostTransportRegistry,
    /// Phase 13.B — voice-session registry. Owns the per-session
    /// jitter buffer + lifecycle state. Always present (cheap to
    /// construct); no HTTP route depends on it being None vs Some.
    pub voice_sessions: crate::voice_session::VoiceSessionRegistry,
    /// Phase 13.C — voice runtime orchestrator. Bridges the
    /// registry's ordered chunks to STT/TTS clients + emits
    /// `VoiceTranscript` / `VoiceAudioOutbound` events. Always
    /// present (mock factories in tests, real Whisper/Kokoro
    /// resolver in production).
    pub voice_runtime: crate::voice_runtime::VoiceRuntime,
    /// 2026-04-28 — per-conversation cancellation flags. The streaming
    /// chat handler registers a flag at turn start and polls it on
    /// every SSE chunk; `POST /api/chats/:id/stop` flips the flag.
    /// Always present — registry is a cheap DashMap behind an Arc.
    pub turn_cancel: crate::turn_cancel::TurnCancellationRegistry,
    /// Phase 16 — per-principal-group runner supervisor. `None`
    /// when the operator hasn't enabled the runner stack
    /// (`RUNNERS_ENABLED=0` or build-time disabled). When `Some`,
    /// the chat path forwards turns to the runner instead of
    /// running them in-process.
    pub runner_supervisor: Option<crate::runner_supervisor::RunnerSupervisor>,
    /// C6c — handle to the deep-research supervisor so the
    /// `/api/admin/research/jobs/{id}/cancel` endpoint can reach
    /// `cancel_tokens` and short-circuit the live gather phase.
    /// `None` only in test fixtures that don't construct the
    /// supervisor (production cmd_serve always wires this up).
    pub research_supervisor: Option<crate::research::ResearchSupervisor>,
    /// Phase C — sink the chat handler enqueues into at successful
    /// turn completion. Never blocks; auto-capture is best-effort
    /// and gated by `config_skills.auto_capture_enabled`. Defaults
    /// to a no-op sink in tests; production constructs the real
    /// sink + spawns the worker via
    /// `crate::skill_capture_runtime::spawn_capture_worker`.
    pub skill_capture: execlaw_skills::AutoCaptureSink,
    /// Phase D.3 — sink for the reuse-update worker. Receives one
    /// request per closed `skill_invocations` row at chat-turn end.
    /// Same noop default + production-spawn pattern as
    /// `skill_capture`.
    pub reuse_update: execlaw_skills::ReuseUpdateSink,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("config", &self.config)
            .finish()
    }
}
