//! execlaw-core
//!
//! The durability heart of execlaw. See `README.md` and MIGRATION_PLAN.md §2
//! for the design.
//!
//! Keep this crate free of network I/O and vendor SDKs. It owns:
//!
//! - SQLite connection pool + SQLCipher key loading
//! - Schema migration runner
//! - Event log primitives
//! - Conversation / principal / outbox / alert data models
//!
//! Anything that talks to Docker, the inference backend, or a transport
//! belongs in a sibling crate.

#![forbid(unsafe_code)]

pub mod alerts;
pub mod attachments;
pub mod config;
pub mod conversation;
pub mod db;
pub mod events;
pub mod ids;
pub mod logs;
pub mod memory;
pub mod migrations;
pub mod outbox;
pub mod principal;
pub mod research;
pub mod transport_cursor;
pub mod vault_row;

pub use db::{Database, DbConfig, DbError};
pub use events::{EventKind, EventLog, EventRecord};
pub use ids::{
    AlertId, AttachmentId, ConversationId, DeploymentId, EventSeq, IdempotencyKey, IncidentId,
    PluginId, PrincipalId, ResearchJobId, TurnSeq,
};
pub use migrations::{MigrationError, MigrationRunner};
