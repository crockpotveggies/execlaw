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

#[cfg(test)]
mod bench_phase2;
#[cfg(test)]
mod bench_phase3;
#[cfg(test)]
mod bench_phase4;

pub use client::{
    GatewayClient, GatewayError, KernelId, KernelInfo, DEFAULT_EXECUTE_TIMEOUT, MAX_OUTPUT_BYTES,
};
pub use hydration::{
    hydrate_uploads, uploads_dir, AttachmentToHydrate, HydratedFile, HydrateOpts, HydrationError,
};
pub use output_watcher::{OutputCreated, OutputWatcher, WatchError, DEFAULT_DEBOUNCE};
pub use jupyter_protocol::{ExecutionState, JupyterEnvelope, JupyterHeader, KernelChannel, MsgType};
pub use kernel_pool::{KernelPool, DEFAULT_IDLE_TIMEOUT};
pub use mime::{
    mime_bundle_from_jupyter_data, ExecuteOutput, ExecuteResult, ExecuteStatus, MimeBundle,
    StreamName,
};
