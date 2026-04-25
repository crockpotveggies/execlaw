//! execlaw-server
//!
//! Axum HTTP + WebSocket server. Exposes:
//!
//! - `GET  /api/health`          — liveness probe, always OK when process is up
//! - `POST /api/setup`           — first-run admin password + controller keypair
//! - `POST /api/login`           — admin password → JWT + refresh cookie
//! - `POST /api/token/refresh`   — rotate refresh token
//! - `POST /api/logout`          — invalidate refresh token
//! - `GET  /api/openapi.json`    — OpenAPI 3 spec (generated via `utoipa`)
//! - `GET  /api/asyncapi.json`   — AsyncAPI 3 spec (hand-authored)
//! - `GET  /api/docs`            — Swagger UI + AsyncAPI viewer bundle
//!
//! JWT signing uses Ed25519 (EdDSA). No cloud dependencies.

#![forbid(unsafe_code)]

pub mod approvals;
pub mod auth;
pub mod auth_extract;
pub mod capability;
pub mod chats;
pub mod docs;
pub mod events;
pub mod observability;
pub mod plugins;
pub mod routes;
pub mod state;
pub mod tool_dispatch;
pub mod tracing_layer;

pub use auth::{JwtSigner, RefreshStore};
pub use events::{EventBus, UiEvent};
pub use state::{AppState, ServerConfig};
