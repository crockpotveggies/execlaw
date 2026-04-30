//! Deep-research subsystem (C3-C6).
//!
//! Three orthogonal pieces — same as the [research handoff doc] —
//! land here:
//!
//!   * [`runner`] — long-running tokio actor that drives a single
//!     job through its phases. C3 ships plan-only; gather (C4) and
//!     synthesize (C5) follow.
//!   * [`supervisor`] — wakes on a tick, picks up `Pending` rows from
//!     `state_research_jobs`, and spawns runners for each.
//!   * Card emit helpers (re-exported from `crate::cards`) — every
//!     job is mirrored as a `Card` so the chat-pane render and the
//!     transport adapters see the same lifecycle every other long-
//!     running tool surfaces through.
//!
//! The job-vs-subagent line is owned by `core::tool::SubagentApi`
//! (synchronous, in-turn, context-isolated) and `core::tool::
//! ResearchApi` (asynchronous, persistent, operator-visible). This
//! module is the server-side runtime that backs the latter.
//!
//! [research handoff doc]: ../../../docs/handoffs/2026-04-29-research-subsystem.md

pub mod runner;
pub mod supervisor;
pub mod workspace;

pub use runner::{ResearchRunnerError, run_job};
pub use supervisor::ResearchSupervisor;
pub use workspace::{ResearchWorkspace, WorkspaceError};
