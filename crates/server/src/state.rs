//! Shared application state.

use crate::events::EventBus;
use execlaw_core::Database;
use execlaw_inference_api::InferenceClient;
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
    /// Default inference-backend URL used when `config_runner_deployments`
    /// has no active Standard-purpose row — falls back to this for
    /// `POST /api/chats/:id/messages`. `None` means "no inference
    /// backend configured; fall back to the Phase 0 stub echo reply".
    ///
    /// Production reads this from the deployment registry (§5.4);
    /// Phase 1 ships the override path for dev + tests.
    pub inference_base_url: Option<String>,
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
            bind_addr: "127.0.0.1:3030".parse().expect("valid default addr"),
            jwt_issuer: "execlaw".to_owned(),
            access_token_ttl_secs: 15 * 60, // 15 minutes, §7.1
            refresh_token_ttl_secs: 7 * 24 * 60 * 60, // 7 days, §7.1
            inference_base_url: None,
            system_prompt: "You are execlaw, a self-hosted agent.".to_owned(),
            model_id: "QuantTrio/Qwen3.5-27B-AWQ".to_owned(),
            max_tool_rounds: 3,
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
    /// OpenAI-compatible client, constructed from `config.inference_base_url`.
    /// `None` when no backend is configured; chat route falls back to
    /// the Phase 0 stub reply.
    pub inference: Option<Arc<InferenceClient>>,
    /// Plugin lifecycle manager + hook registry. Every route that
    /// enumerates tools / transports / UI panels reads from here.
    pub plugin_host: PluginHost,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("config", &self.config)
            .finish()
    }
}
