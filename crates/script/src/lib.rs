//! execlaw-script — embedded Rhai interpreter for the script-tier
//! plugin runtime.
//!
//! ## Why this exists
//!
//! Most plugins on the inventory are HTTP-API wrappers (Google
//! Contacts/Calendar, Slack, GitHub, etc.). Shipping each as a
//! compiled Rust binary means a 2.88 MB artifact + multi-minute cold
//! compile per plugin — wasteful for what is essentially a config
//! file.
//!
//! The script tier replaces that with a `.rhai` source file (~50
//! lines for google-contacts) loaded into an in-process Rhai
//! interpreter under a sandbox. Install ZIP is the script + the
//! manifest. "Compile" is parsing the script. Hot-reloadable.
//!
//! ## Sandbox
//!
//! - `Engine::set_max_operations(1_000_000)` per call — runaway
//!   loops abort with `OperationsLimit`.
//! - `Engine::set_max_call_levels(64)` — bounded recursion.
//! - `Engine::set_max_array_size` / `set_max_string_size` /
//!   `set_max_map_size` — bounded growth.
//! - The engine never sees `eval`, `import`, or any FFI hook. The
//!   only "outside world" access is the host-provided primitives
//!   registered via [`register_primitives`].
//!
//! ## Primitives the host injects
//!
//! Just enough to be useful, not so many we recreate Node:
//!
//! - **HTTP**: `http_get(url, query, bearer)` /
//!   `http_post(url, body, bearer)` /
//!   `http_get_cached(url, query, bearer, ttl_secs)`
//! - **Strings**: `digits_only(s)`, `lower(s)`, `trim(s)`,
//!   `hash(s)` (FNV-1a → hex)
//! - **JSON path**: `json_path(value, path)` (RFC 9535)
//! - **Time**: `now()` (unix seconds)
//! - **Logging**: `log_info(msg)`, `log_warn(msg)` — into
//!   tracing
//!
//! Everything else (file I/O, network beyond `http_*`, processes)
//! is **not in scope** of the script.

#![forbid(unsafe_code)]

mod cache;
mod engine;
mod errors;
mod host_caps;
mod plugin;
mod primitives;

pub use engine::ScriptEngine;
pub use errors::{ScriptError, ScriptResult};
pub use host_caps::{
    AttachmentBytes, HostCapError, HostCapabilities, HostCapabilitiesArc, InboundAttachmentMeta,
    InboundMessage, RouteOutcome, WsFrameHandler, WsSubscriptionHandle,
};
pub use plugin::ScriptPlugin;

/// Re-exports for callers outside the script crate that need to
/// shuttle data into the engine — chiefly the admin-routes
/// dispatcher in `execlaw-server` which builds a Rhai args map
/// from an HTTP request.
pub mod primitives_glue {
    pub use crate::primitives::{json_to_rhai, rhai_to_json};
}
