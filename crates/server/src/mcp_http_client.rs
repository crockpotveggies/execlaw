//! Minimal Streamable HTTP MCP client for execlaw.
//!
//! The MCP Streamable HTTP transport is JSON-RPC-2.0 over HTTP. The
//! spec allows servers to upgrade a response to an SSE stream; for
//! v1 we only handle the simple request/response path (single JSON
//! body), which is what every public MCP server returns in
//! practice (Atlassian Rovo, GitHub Copilot, etc.). SSE streaming
//! is a follow-up.
//!
//! Public surface mirrors `execlaw_mcp_client::McpClient`:
//!   * `connect(url, bearer).await -> HttpMcpClient` — runs the
//!     initialize + initialized handshake, returns a handle.
//!   * `list_tools()`, `call_tool(name, args)` — same shapes as
//!     stdio so the caller (`mcp_host`) can dispatch by transport
//!     and pretend they're the same.
//!
//! Auth: bearer token in `Authorization: Bearer <token>`. OAuth 2.1
//! dynamic client registration is a deferred follow-up; for v1 the
//! operator (or the agent on the operator's behalf) supplies a
//! pre-issued API token.

use execlaw_mcp_client::{CallToolResult, McpError, McpResult, McpTool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info};

/// MCP protocol version we negotiate with the server. Mirrors the
/// constant in execlaw-mcp-client.
const PROTOCOL_VERSION: &str = "2025-06-18";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize)]
struct RpcEnvelope<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

/// One Streamable-HTTP MCP connection. Cheap to clone — the inner
/// reqwest client is reusable across calls.
#[derive(Clone)]
pub struct HttpMcpClient {
    http: reqwest::Client,
    url: String,
    bearer: Option<String>,
    next_id: std::sync::Arc<AtomicU64>,
}

impl HttpMcpClient {
    /// Open a new connection. Sends `initialize` + `initialized`
    /// notification per the MCP spec; returns a ready handle.
    pub async fn connect(url: &str, bearer: Option<&str>) -> McpResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("execlaw/", env!("CARGO_PKG_VERSION"), "/mcp-http"))
            .build()
            .map_err(|e| McpError::Protocol(format!("http client build: {e}")))?;
        let me = Self {
            http,
            url: url.to_owned(),
            bearer: bearer.map(|s| s.to_owned()),
            next_id: std::sync::Arc::new(AtomicU64::new(0)),
        };

        // Initialize handshake.
        let init_params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "execlaw",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let init: Value = me.call("initialize", Some(init_params)).await?;
        let server_name = init
            .get("serverInfo")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let protocol = init
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        info!(server = %server_name, protocol = %protocol, url = %url, "MCP server initialized over HTTP");

        // Tell the server we're ready. Notifications have no id +
        // no response.
        me.notify("notifications/initialized", None).await?;

        Ok(me)
    }

    /// `tools/list` — same return shape as the stdio client.
    pub async fn list_tools(&self) -> McpResult<Vec<McpTool>> {
        let result: Value = self.call("tools/list", None).await?;
        let tools = result.get("tools").cloned().ok_or_else(|| {
            McpError::Protocol("tools/list response missing `tools` field".into())
        })?;
        let parsed: Vec<McpTool> = serde_json::from_value(tools)
            .map_err(|e| McpError::Protocol(format!("decode tools/list: {e}")))?;
        Ok(parsed)
    }

    /// `tools/call` — same return shape as the stdio client.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> McpResult<CallToolResult> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result: Value = self.call("tools/call", Some(params)).await?;
        let parsed: CallToolResult = serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("decode tools/call: {e}")))?;
        Ok(parsed)
    }

    /// Generic JSON-RPC request. Returns the `result` field on
    /// success; surfaces RPC errors verbatim.
    async fn call(&self, method: &str, params: Option<Value>) -> McpResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let env = RpcEnvelope {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&env);
        if let Some(tok) = &self.bearer {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| McpError::Protocol(format!("http {method}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(McpError::Protocol(format!(
                "http {method} returned {}: {}",
                status.as_u16(),
                truncate(&body, 240)
            )));
        }

        // Streamable HTTP can respond as single-shot JSON OR an SSE
        // stream. We detect by Content-Type. v1 only handles the
        // single-shot path (every public MCP server uses it for
        // basic request/response); SSE streaming is a follow-up.
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ctype.contains("text/event-stream") {
            // Walk the stream looking for the FIRST data: line that
            // is a complete JSON-RPC response with our id. Slack-
            // simple parser; not production-grade but covers Rovo +
            // the other servers I've probed.
            let body = resp
                .text()
                .await
                .map_err(|e| McpError::Protocol(format!("http {method} read sse: {e}")))?;
            for chunk in body.split("\n\n") {
                for line in chunk.lines() {
                    let payload = match line.strip_prefix("data: ") {
                        Some(p) => p,
                        None => continue,
                    };
                    let candidate: RpcResponse = match serde_json::from_str(payload) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    if candidate.id.as_ref().and_then(|v| v.as_u64()) != Some(id) {
                        continue;
                    }
                    return rpc_to_result(method, candidate);
                }
            }
            return Err(McpError::Protocol(format!(
                "sse {method}: no matching response in stream"
            )));
        }

        let parsed: RpcResponse = resp
            .json()
            .await
            .map_err(|e| McpError::Protocol(format!("http {method} decode: {e}")))?;
        rpc_to_result(method, parsed)
    }

    /// Fire-and-forget JSON-RPC notification (no id, no response
    /// expected). Servers typically return 200 / 202 with empty body.
    async fn notify(&self, method: &str, params: Option<Value>) -> McpResult<()> {
        let env = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&env);
        if let Some(tok) = &self.bearer {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| McpError::Protocol(format!("http {method}: {e}")))?;
        if !resp.status().is_success() {
            let s = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            // Notifications: 4xx is informational only. Don't fail
            // the connection over a server that returns 405 on
            // notifications/initialized — the call() path is what
            // matters.
            debug!(method, status = %s, body = %truncate(&body, 120), "notification non-2xx");
        }
        Ok(())
    }
}

fn rpc_to_result(method: &str, resp: RpcResponse) -> McpResult<Value> {
    if let Some(err) = resp.error {
        return Err(McpError::Protocol(format!(
            "rpc {method} -> code={} {}",
            err.code, err.message
        )));
    }
    resp.result.ok_or_else(|| {
        McpError::Protocol(format!(
            "rpc {method}: response had neither result nor error"
        ))
    })
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_to_result_returns_err_on_rpc_error() {
        let resp = RpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(0)),
            result: None,
            error: Some(RpcError {
                code: -32603,
                message: "internal".to_string(),
                data: None,
            }),
        };
        let err = rpc_to_result("initialize", resp).unwrap_err();
        assert!(format!("{err}").contains("-32603"));
    }

    #[test]
    fn rpc_to_result_returns_ok_on_result() {
        let resp = RpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(0)),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let v = rpc_to_result("ping", resp).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn truncate_trims_long_strings() {
        let s = "a".repeat(300);
        let out = truncate(&s, 100);
        assert_eq!(out.chars().count(), 101);
        assert!(out.ends_with('…'));
    }
}
