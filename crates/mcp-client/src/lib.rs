//! `execlaw-mcp-client` — Model Context Protocol client.
//!
//! Phase 8b of the execlaw plan. Lets the operator point execlaw at
//! one or more existing MCP servers (github-mcp, slack-mcp, anything
//! they already run) and have those tools surface in the runner's
//! tool array. Phase 8c wires this into a connection manager that
//! reflects the tool list into `config_tool_access` rows; Phase 8d
//! routes dispatch through it.
//!
//! Locked decisions baked into this crate:
//!   * Sampling is refused on every connection — execlaw never lets
//!     a server make us run an LLM call.
//!   * Tools and resources are first-class. Prompts (the third MCP
//!     primitive) are deliberately out of scope.
//!   * Stdio + Streamable HTTP are the supported transports. Stdio
//!     ships first (8b); HTTP joins in 8c with the connection
//!     manager.

#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod protocol;
pub mod stdio;

pub use client::{McpClient, McpNotification, REQUEST_TIMEOUT};
pub use error::{McpError, McpResult};
pub use protocol::{
    CallToolResult, McpResource, McpTool, ReadResourceResult, ServerCapabilities, ServerInfo,
};
pub use stdio::StdioSpec;
