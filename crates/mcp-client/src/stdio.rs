//! Stdio transport for MCP. Spawns a child process, writes one JSON
//! frame per line on its stdin, reads one frame per line from its
//! stdout. Stderr is forwarded to tracing — MCP servers traditionally
//! use stderr for human-readable logs.
//!
//! The transport is thin: framing only. The `McpClient` actor in
//! `client.rs` owns the request/response correlation, the
//! initialize handshake, and the refusal of server-initiated
//! sampling requests.

use crate::error::{McpError, McpResult};
use crate::protocol::{InboundFrame, RpcRequest, RpcNotification};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tracing::{debug, warn};

/// One-line JSON-RPC frame. Stdio MCP servers MUST emit one frame
/// per line on stdout (per spec); we enforce the same on writes.
fn frame_line(value: &impl serde::Serialize) -> McpResult<String> {
    let mut s = serde_json::to_string(value)
        .map_err(|e| McpError::Protocol(format!("encoding frame: {e}")))?;
    if s.contains('\n') {
        return Err(McpError::Protocol(
            "JSON frame contains an embedded newline; would corrupt the line stream".into(),
        ));
    }
    s.push('\n');
    Ok(s)
}

/// Minimal stdio transport. The actor in `client.rs` owns this and
/// drives reads/writes from a single task — no shared state needed
/// between caller and transport.
pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Spec for spawning an stdio MCP server. Mirrors the
/// `config_mcp_servers` columns Phase 8c will populate.
#[derive(Debug, Clone)]
pub struct StdioSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<std::path::PathBuf>,
}

impl StdioTransport {
    pub async fn spawn(spec: &StdioSpec) -> McpResult<Self> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Spawn(format!("{}: {e}", spec.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Spawn("child stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Spawn("child stdout not piped".into()))?;
        if let Some(stderr) = child.stderr.take() {
            // Forward stderr lines to tracing as warn — keeps the
            // test-harness output readable without dropping the data.
            tokio::spawn(async move {
                let mut buf = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = buf.next_line().await {
                    if !line.is_empty() {
                        warn!(target: "mcp_client::stderr", "{line}");
                    }
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    pub async fn write_request(&mut self, req: &RpcRequest) -> McpResult<()> {
        let line = frame_line(req)?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(McpError::Io)?;
        self.stdin.flush().await.map_err(McpError::Io)
    }

    pub async fn write_notification(&mut self, n: &RpcNotification) -> McpResult<()> {
        let line = frame_line(n)?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(McpError::Io)?;
        self.stdin.flush().await.map_err(McpError::Io)
    }

    /// Read one frame. Returns `None` on EOF (server exited cleanly).
    pub async fn read_frame(&mut self) -> McpResult<Option<InboundFrame>> {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).await.map_err(McpError::Io)?;
        if n == 0 {
            return Ok(None);
        }
        debug!(target: "mcp_client::stdout", "{line}");
        let frame: InboundFrame = serde_json::from_str(line.trim_end())
            .map_err(|e| McpError::Protocol(format!("decoding inbound frame: {e}")))?;
        Ok(Some(frame))
    }

    /// Mutable accessor for stdin. The client actor uses this to
    /// write structured error responses for refused server-initiated
    /// requests without going through the JSON-RPC `RpcRequest`
    /// builder (which expects an outbound REQUEST id, not a
    /// response id). Internal — no need for callers outside the
    /// crate.
    pub(crate) fn stdin_mut(&mut self) -> &mut ChildStdin {
        &mut self.stdin
    }

    /// Best-effort shutdown — closes stdin (which signals EOF to the
    /// child) and waits briefly for it to exit. Drop will SIGKILL if
    /// the child ignores us.
    pub async fn shutdown(mut self) -> McpResult<()> {
        let _ = self.stdin.shutdown().await;
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.child.wait(),
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{RpcId, RpcRequest};

    #[test]
    fn frame_line_appends_single_newline() {
        let r = RpcRequest::new(RpcId::Int(1), "ping", None);
        let s = frame_line(&r).unwrap();
        assert!(s.ends_with('\n'));
        assert_eq!(s.matches('\n').count(), 1);
    }

    #[test]
    fn frame_line_rejects_embedded_newline() {
        // Construct a payload that round-trips through JSON with no
        // newlines — serde_json never emits raw newlines for a normal
        // string. To exercise the embedded-newline branch we'd need a
        // pretty-printed value, which we never produce. Instead pin
        // the negative invariant: serde_json::to_string is single-line.
        let r = RpcRequest::new(
            RpcId::Int(1),
            "echo",
            Some(serde_json::json!({"k": "v\nwith embedded"})),
        );
        let s = frame_line(&r).unwrap();
        // Only the trailing newline; the embedded one is escaped to "\n" in JSON.
        assert_eq!(s.matches('\n').count(), 1);
    }
}
