//! Python sandbox host module — drives the `python-sandbox` plugin's
//! kernel gateway sidecar.
//!
//! Architecture (audit-approved): one shared container running
//! `jupyter_kernel_gateway`, kernel-per-conversation managed via the
//! gateway's HTTP+WS API. The sidecar supervisor keeps the container
//! alive; THIS module owns per-conversation kernel lifecycle.
//!
//! Boundaries:
//!   - This module knows about kernels, the gateway, and the Jupyter
//!     wire protocol.
//!   - It does NOT know about `state_conversations`, `state_attachments`,
//!     trust classes, or message bus state. Tool dispatchers (Phase 8)
//!     glue this module to those concerns.
//!   - It does NOT manage the kernel-gateway container's lifecycle;
//!     that's [`crate::sidecar_supervisor`].
//!
//! Public surface:
//!   - [`GatewayClient`] — HTTP+WS client to the kernel gateway sidecar.
//!   - [`MimeBundle`] — execlaw-shaped MIME bundle returned to tool
//!     callers and to the SPA's chip renderer.
//!   - [`ExecuteResult`] — the shape `python.execute` returns.
//!   - [`KernelId`] — strongly-typed wrapper over the gateway's kernel
//!     UUID; deliberately distinct from execlaw's other id types so the
//!     compiler catches "I passed a conversation_id where a kernel_id
//!     belongs" before runtime.
//!
//! Phase 2a (this slice): types + HTTP lifecycle (create/delete/interrupt/
//! restart/list). WS execute, kernel pool, idle eviction, and tool
//! dispatchers land in Phase 2b/7/8.

pub mod client;
pub mod hydration;
pub mod jupyter_protocol;
pub mod kernel_pool;
pub mod mime;
pub mod output_watcher;
pub mod service;
pub mod tools;
pub mod wiring;

#[cfg(test)]
mod adversarial;
#[cfg(test)]
mod bench_phase2;
#[cfg(test)]
mod bench_phase3;
#[cfg(test)]
mod bench_phase4;

pub use client::{
    DEFAULT_EXECUTE_TIMEOUT, GatewayClient, GatewayError, KernelId, KernelInfo, MAX_OUTPUT_BYTES,
};
pub use hydration::{
    AttachmentToHydrate, HydrateOpts, HydratedFile, HydrationError, hydrate_uploads, uploads_dir,
};
pub use jupyter_protocol::{
    ExecutionState, JupyterEnvelope, JupyterHeader, KernelChannel, MsgType,
};
pub use kernel_pool::{DEFAULT_IDLE_TIMEOUT, KernelPool};
pub use mime::{
    ExecuteOutput, ExecuteResult, ExecuteStatus, MimeBundle, StreamName,
    mime_bundle_from_jupyter_data,
};
pub use output_watcher::{DEFAULT_DEBOUNCE, OutputCreated, OutputWatcher, WatchError};
pub use service::{PLUGIN_ID, PythonSandboxService, PythonSandboxSettings, ServiceError};
pub use tools::{
    PythonExecuteTool, PythonInterruptTool, PythonListFilesTool, PythonResetTool,
    python_sandbox_tools,
};
pub use wiring::{WireError, wire_python_sandbox};

// ----------------------------------------------------------------
// Process-wide service handle. 2026-05-18.
//
// `wire_python_sandbox` constructs the service in a spawned task
// after the supervisor has the kernel-gateway sidecar healthy
// (the wiring runs ~2s after axum::serve() comes up so it can wait
// for fire_on_enable_for_all to spawn the container). The service
// then needs to be reachable from request handlers — specifically
// the `DELETE /api/chats/{id}` handler, which calls
// `on_conversation_deleted` to delete `/work/<convo>/` from the
// sidecar's bind-mount.
//
// AppState can't own the field directly because AppState is built
// BEFORE wiring runs (the chicken-and-egg: AppState needs the
// supervisor to be wired into routes, but wiring needs AppState to
// be alive to read the supervisor handle). A process-wide
// OnceLock keeps the API minimal: cli/main.rs calls
// `set_service(svc)` exactly once after wiring succeeds; any
// handler reads `service()` and degrades gracefully when None
// (plugin not installed, sidecar not healthy, wiring failed).
//
// The OnceLock ALSO doubles as a liveness anchor for the service's
// OutputWatcher (notify OS thread + tokio timer task). Without
// something holding the Arc, Drop would run as soon as the
// spawned-wiring-task exited and the watcher would stop publishing
// streamed artifacts mid-execution. Single source of truth here
// means we don't need a separate "keep this alive" static in
// cli/main.rs anymore.

use std::sync::Arc;
use std::sync::OnceLock;

static SERVICE: OnceLock<Arc<PythonSandboxService>> = OnceLock::new();

/// Set the process-wide python-sandbox service handle. Called once
/// from `cli/main.rs` after `wire_python_sandbox` returns a live
/// service. Subsequent calls are silently ignored (OnceLock
/// semantics) so a bug that calls this twice won't panic — but
/// also won't replace the handle.
pub fn set_service(svc: Arc<PythonSandboxService>) {
    let _ = SERVICE.set(svc);
}

/// Read the process-wide python-sandbox service handle.
///
/// Returns `None` when:
///   * the plugin isn't installed (boot wiring returned None)
///   * the kernel-gateway sidecar never became healthy
///   * wiring errored out
///   * called from a test that doesn't boot the full server
///
/// Callers MUST degrade gracefully to "skip the sandbox-side
/// work" rather than fail the request. The delete-thread handler,
/// for instance, still deletes the conversation row + events even
/// when the sidecar work-dir cleanup can't run.
pub fn service() -> Option<Arc<PythonSandboxService>> {
    SERVICE.get().cloned()
}
