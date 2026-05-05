//! MCP wire types — JSON-RPC 2.0 framing + the MCP-specific method
//! payloads execlaw cares about.
//!
//! The protocol is JSON-RPC 2.0 with three flavours of frame:
//!   * **Request** — has an `id`, expects a matching `Response`.
//!   * **Response** — has the same `id`, carries `result` xor `error`.
//!   * **Notification** — no `id`, fire-and-forget.
//!
//! We keep the types narrow on purpose. MCP defines a lot more
//! optional fields than execlaw uses; deserialising with `serde`'s
//! default (deny-unknown disabled) means extra fields the server
//! sends are simply ignored.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request id. Spec allows string / number / null;
/// execlaw mints integers monotonically per connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RpcId {
    Int(u64),
    Str(String),
}

impl From<u64> for RpcId {
    fn from(v: u64) -> Self {
        Self::Int(v)
    }
}

/// Outbound request frame.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    pub jsonrpc: &'static str,
    pub id: RpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    pub fn new(id: RpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// Outbound notification (no id; no response expected).
#[derive(Debug, Clone, Serialize)]
pub struct RpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.into(),
            params,
        }
    }
}

/// Inbound frame — could be a response (matching one of our request
/// ids), a server-initiated request, or a notification.
///
/// JSON-RPC discriminates on the presence of `id` + `method`:
///   * `id` + `result`/`error` → response
///   * `id` + `method` → server-initiated request (rare; sampling)
///   * `method` only → notification
#[derive(Debug, Clone, Deserialize)]
pub struct InboundFrame {
    #[allow(dead_code)]
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<RpcId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC standard error codes. We only emit a few; most go through
/// the `RpcError::message` for human-readable forwarding.
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

// ---------------------------------------------------------------------------
// MCP method payloads — only the ones execlaw uses are typed.
// ---------------------------------------------------------------------------

/// `initialize` request params. Our protocol-version string matches
/// what most current MCP servers expect (`2024-11-05`); newer servers
/// negotiate downward gracefully.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Serialize)]
pub struct InitializeParams<'a> {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'a str,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo<'a>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientCapabilities {
    /// Roots tell the server which filesystem trees are visible.
    /// We never declare any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    /// `sampling` empty-object means "client supports sampling
    /// requests"; we deliberately leave it `None` so servers know
    /// not to ask. Our refusal of `sampling/createMessage` is
    /// belt-and-suspenders defense.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RootsCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct InitializeResult {
    #[serde(default, rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(default, rename = "serverInfo")]
    pub server_info: Option<ServerInfo>,
    /// Optional human-readable instructions servers can include for
    /// the client to surface in UI. Treated as opaque metadata.
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub resources: Option<Value>,
    #[serde(default)]
    pub prompts: Option<Value>,
    #[serde(default)]
    pub logging: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// `tools/list` result.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<McpTool>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// One tool exposed by the server. Schema is opaque JSON-Schema.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

/// `tools/call` request params.
#[derive(Debug, Clone, Serialize)]
pub struct CallToolParams<'a> {
    pub name: &'a str,
    pub arguments: Value,
}

/// `tools/call` result. The MCP spec returns a `content` array of
/// typed parts (text / image / resource link) plus an `isError`
/// boolean. We pass the raw structure through so callers can decide
/// how to format it for the LLM.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

/// `resources/list` result.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListResourcesResult {
    #[serde(default)]
    pub resources: Vec<McpResource>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpResource {
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
}

/// `resources/read` request params.
#[derive(Debug, Clone, Serialize)]
pub struct ReadResourceParams<'a> {
    pub uri: &'a str,
}

/// `resources/read` result. `contents` is a list of typed parts (text
/// or blob); we keep them raw.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReadResourceResult {
    #[serde(default)]
    pub contents: Vec<Value>,
}

/// Method names — kept here so typos surface at compile time.
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const RESOURCES_LIST: &str = "resources/list";
    pub const RESOURCES_READ: &str = "resources/read";

    /// Refused server-initiated request — execlaw never lets a server
    /// trigger an LLM sample on its behalf.
    pub const SAMPLING_CREATE_MESSAGE: &str = "sampling/createMessage";
}

/// Notification method names.
pub mod notifications {
    pub const INITIALIZED: &str = "notifications/initialized";
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rpc_request_serialises_to_canonical_json() {
        let req = RpcRequest::new(
            RpcId::Int(7),
            "tools/call",
            Some(serde_json::json!({"name": "x", "arguments": {}})),
        );
        let s = serde_json::to_string(&req).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "tools/call");
        assert_eq!(v["params"]["name"], "x");
    }

    #[test]
    fn rpc_notification_omits_id() {
        let n = RpcNotification::new(notifications::INITIALIZED, None);
        let s = serde_json::to_string(&n).unwrap();
        assert!(!s.contains("\"id\""));
        assert!(s.contains("notifications/initialized"));
    }

    #[test]
    fn inbound_frame_distinguishes_response_request_notification() {
        let response: InboundFrame =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"x":1}}"#).unwrap();
        assert!(response.id.is_some());
        assert!(response.method.is_none());
        assert!(response.result.is_some());

        let notif: InboundFrame =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(notif.id.is_none());
        assert!(notif.method.is_some());

        let server_req: InboundFrame = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":42,"method":"sampling/createMessage","params":{}}"#,
        )
        .unwrap();
        assert!(server_req.id.is_some());
        assert!(server_req.method.is_some());
    }

    #[test]
    fn list_tools_result_parses_minimal_payload() {
        let r: ListToolsResult = serde_json::from_str(
            r#"{"tools":[{"name":"create_pr","inputSchema":{"type":"object"}}]}"#,
        )
        .unwrap();
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].name, "create_pr");
        assert!(r.tools[0].input_schema.is_some());
    }

    #[test]
    fn call_tool_result_default_is_error_false() {
        let r: CallToolResult =
            serde_json::from_str(r#"{"content":[{"type":"text","text":"ok"}]}"#).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 1);
    }

    #[test]
    fn unknown_fields_in_inbound_are_silently_ignored() {
        // Real-world MCP servers ship extension fields; we must
        // tolerate them rather than failing the whole frame.
        let frame: InboundFrame = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"result":{"foo":"bar"},"_meta":{"trace":"x"}}"#,
        )
        .unwrap();
        assert!(frame.error.is_none());
        assert!(frame.result.is_some());
    }
}
