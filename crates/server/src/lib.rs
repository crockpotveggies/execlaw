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

pub mod alerts;
pub mod approvals;
pub mod auth;
pub mod auth_extract;
pub mod backend_presets;
pub mod backend_supervisor;
pub mod backends;
pub mod capability;
pub mod chats;
pub mod docs;
pub mod events;
pub mod inference_resolver;
pub mod mcp_admin;
pub mod mcp_host;
pub mod my_identities;
pub mod observability;
pub mod personality;
pub mod plugins;
pub mod routes;
pub mod routine_runner;
pub mod routines;
pub mod runner_registry;
pub mod runners_admin;
pub mod state;
pub mod tool_dispatch;
pub mod tool_sync;
pub mod tools_admin;
pub mod tracing_layer;
pub mod trust_policy;
pub mod users;
pub mod voice_clients;
pub mod voice_frame;
pub mod voice_reaper;
pub mod voice_runtime;
pub mod voice_session;
pub mod webauthn;

pub use auth::{JwtSigner, RefreshStore};
pub use events::{EventBus, UiEvent};
pub use state::{AppState, ServerConfig};
