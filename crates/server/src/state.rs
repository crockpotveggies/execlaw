//! Shared application state.

use crate::events::EventBus;
use execlaw_core::Database;
use std::sync::Arc;

/// Configuration for the server process.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: std::net::SocketAddr,
    /// Issuer string placed in JWT claims.
    pub jwt_issuer: String,
    pub access_token_ttl_secs: i64,
    pub refresh_token_ttl_secs: i64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:3030".parse().expect("valid default addr"),
            jwt_issuer: "execlaw".to_owned(),
            access_token_ttl_secs: 15 * 60, // 15 minutes, §7.1
            refresh_token_ttl_secs: 7 * 24 * 60 * 60, // 7 days, §7.1
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
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("config", &self.config)
            .finish()
    }
}
