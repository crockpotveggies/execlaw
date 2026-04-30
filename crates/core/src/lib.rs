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
pub mod audit;
pub mod backends;
pub mod builtin_tools;
pub mod cards;
pub mod config;
pub mod conversation;
pub mod db;
pub mod ephemeral_sweeper;
pub mod eval;
pub mod event_hmac;
pub mod event_retention;
pub mod events;
pub mod general_settings;
pub mod ids;
pub mod log_retention;
pub mod logs;
pub mod mcp_servers;
pub mod memory;
pub mod migrations;
pub mod outbox;
pub mod personality;
pub mod principal;
pub mod principal_groups;
pub mod refresh_tokens;
pub mod research;
pub mod retention;
pub mod routine_run_retention;
pub mod routines;
pub mod tool;
pub mod tool_access;
pub mod tool_apis;
pub mod transport_conversations;
pub mod trust_policy;
pub mod transport_cursor;
pub mod users;
pub mod vault_row;
pub mod webauthn;

pub use db::{Database, DbConfig, DbError};
pub use events::{EventKind, EventLog, EventRecord};
pub use ids::{
    AlertId, AttachmentId, ConversationId, EventSeq, IdempotencyKey, IncidentId,
    PluginId, PrincipalId, ResearchJobId, TurnSeq,
};
pub use migrations::{MigrationError, MigrationRunner};
