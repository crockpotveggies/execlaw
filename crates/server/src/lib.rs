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
pub mod attachment_api;
pub mod attachments_admin;
pub mod auth;
pub mod auth_extract;
pub mod backend_presets;
pub mod backend_supervisor;
pub mod backends;
pub mod cards;
pub mod chat_alert;
pub mod chats;
pub mod docs;
pub mod events;
pub mod factory_reset;
pub mod generic_inbound;
pub mod group_addressing;
pub mod host_caps_impl;
pub mod inference_resolver;
pub mod mcp_admin;
pub mod mcp_host;
pub mod mcp_http_client;
pub mod my_identities;
pub mod oauth_admin;
pub mod oauth_provider;
pub mod oauth_sweeper;
pub mod observability;
pub mod personality;
pub mod plugin_admin_routes;
pub mod plugin_lifecycle;
pub mod plugin_settings_admin;
pub mod plugin_webhook_routes;
pub mod plugins;
pub mod principal_admit;
pub mod research;
pub mod research_admin;
pub mod routes;
pub mod routine_runner;
pub mod routines;
pub mod runner_rpc;
pub mod runner_spawn;
pub mod runner_supervisor;
pub mod runners_admin;
pub mod search_rate_limit;
pub mod search_resolver;
pub mod search_rotating;
pub mod settings_general;
pub mod settings_research;
pub mod settings_search;
pub mod setup_preflight;
pub mod sidecar_supervisor;
pub mod sidecars_admin;
pub mod transport_registry;
// Phase B: signal_admin / signal_inbound / signal_tools /
// signal_transport retired. Every Signal capability lives in
// plugins/signal/main.rhai (v0.4.0+) and reaches the sidecar via
// `sidecar_http_*` Rhai bindings; transport methods flow through
// `rhai_transport::RhaiBackedTransport`.
pub mod skill_capture_runtime;
pub mod skills_admin;
pub mod state;
pub mod think_filter;
pub mod tool_apis_http;
pub mod tool_apis_mcp;
pub mod tool_apis_search;
pub mod tool_apis_search_brave;
pub mod tool_apis_search_exa;
pub mod tool_apis_search_perplexity;
pub mod tool_apis_search_searchapi;
pub mod tool_apis_search_searxng;
pub mod tool_apis_search_serpapi;
pub mod tool_apis_search_tavily;
pub mod tool_apis_search_websurfx;
pub mod tool_apis_subagent;
pub mod tool_dispatch;
pub mod tool_sync;
pub mod tools_admin;
pub mod tracing_layer;
pub mod trust_policy;
pub mod turn_cancel;
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
