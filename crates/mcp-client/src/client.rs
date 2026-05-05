//! High-level MCP client.
//!
//! Owns a `StdioTransport` (Phase 8b ships stdio only; HTTP comes
//! with the connection manager in 8c). Spawns one tokio task per
//! connection that:
//!   * Reads inbound frames in a loop.
//!   * Routes responses to pending request waiters via id lookup.
//!   * Forwards notifications on a broadcast channel.
//!   * Refuses every server-initiated request — the only one MCP
//!     defines is `sampling/createMessage`, and our locked decision
//!     is to never let an external server make us run an LLM call.
//!
//! Public API:
//!   * `McpClient::stdio(spec, shutdown).await -> Result<Self>` —
//!     spawns child, runs initialize handshake, returns a handle.
//!   * `client.list_tools().await`
//!   * `client.call_tool(name, args).await`
//!   * `client.list_resources().await`
//!   * `client.read_resource(uri).await`
//!   * `client.subscribe_notifications() -> broadcast::Receiver<_>`

use crate::error::{McpError, McpResult};
use crate::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, InboundFrame, InitializeParams,
    InitializeResult, ListResourcesResult, ListToolsResult, McpResource, McpTool, PROTOCOL_VERSION,
    ReadResourceParams, ReadResourceResult, RpcId, RpcNotification, RpcRequest, ServerCapabilities,
    error_codes, methods, notifications,
};
use crate::stdio::{StdioSpec, StdioTransport};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

/// Default per-request timeout. MCP servers sometimes do real work
/// (network calls, file scans) so this is generous; callers that
/// need tighter budgets can set their own outer timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Map from in-flight request id → its waiter. Aliased so the
/// `&Arc<Mutex<...>>` parameter at the bottom of the file doesn't
/// trip clippy's `type_complexity` lint.
type PendingMap = Mutex<HashMap<RpcId, oneshot::Sender<McpResult<Value>>>>;

/// Notifications surfaced to subscribers. We only model the kinds
/// execlaw cares about; everything else is dropped.
#[derive(Debug, Clone)]
pub enum McpNotification {
    /// `notifications/tools/list_changed` — caller should re-fetch
    /// the tool list and reconcile the registry.
    ToolsListChanged,
    /// `notifications/resources/list_changed` — same shape, for
    /// resource roots.
    ResourcesListChanged,
}

/// Internal request the actor processes. The oneshot returns the
/// raw `result` field (or an error mapped from `RpcError`).
struct PendingCall {
    method: String,
    params: Option<Value>,
    reply: oneshot::Sender<McpResult<Value>>,
}

/// Public handle. Cloneable — every clone shares the same actor.
#[derive(Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicU64,
    requests: mpsc::Sender<PendingCall>,
    notifs: broadcast::Sender<McpNotification>,
    server_capabilities: ServerCapabilities,
}

impl McpClient {
    /// Spawn an stdio MCP server, run the initialize handshake, and
    /// return a handle. The actor runs until `shutdown` is notified
    /// or the child closes its stdout.
    pub async fn stdio(spec: &StdioSpec, shutdown: Arc<tokio::sync::Notify>) -> McpResult<Self> {
        let mut transport = StdioTransport::spawn(spec).await?;

        // Pending request id → oneshot sender map. Built first so the
        // initialize call can use it.
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel::<PendingCall>(32);
        let (notif_tx, _) = broadcast::channel::<McpNotification>(16);
        let notif_tx_actor = notif_tx.clone();
        let next_id = Arc::new(AtomicU64::new(0));

        // ---- Initialize handshake ----
        let init_id = RpcId::Int(next_id.fetch_add(1, Ordering::SeqCst));
        let init_req = RpcRequest::new(
            init_id.clone(),
            methods::INITIALIZE,
            Some(serde_json::to_value(InitializeParams {
                protocol_version: PROTOCOL_VERSION,
                capabilities: ClientCapabilities::default(),
                client_info: ClientInfo {
                    name: "execlaw",
                    version: env!("CARGO_PKG_VERSION"),
                },
            })?),
        );
        transport.write_request(&init_req).await?;
        let init_result: InitializeResult = read_until_response::<InitializeResult>(
            &mut transport,
            &init_id,
            REQUEST_TIMEOUT,
            &notif_tx_actor,
        )
        .await?;
        info!(
            server = %init_result.server_info.as_ref().map(|i| i.name.as_str()).unwrap_or("?"),
            protocol = %init_result.protocol_version,
            "MCP server initialized"
        );
        let server_capabilities = init_result.capabilities.clone();

        // Tell the server we're ready.
        transport
            .write_notification(&RpcNotification::new(notifications::INITIALIZED, None))
            .await?;

        // ---- Spawn the actor ----
        let pending_actor = pending.clone();
        let next_id_actor = next_id.clone();
        tokio::spawn(async move {
            let mut transport = transport;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.notified() => {
                        info!("MCP client shutdown signal received");
                        break;
                    }
                    out = rx.recv() => {
                        match out {
                            Some(call) => {
                                let id = RpcId::Int(next_id_actor.fetch_add(1, Ordering::SeqCst));
                                let req = RpcRequest::new(
                                    id.clone(),
                                    call.method,
                                    call.params,
                                );
                                pending_actor.lock().await.insert(id, call.reply);
                                if let Err(e) = transport.write_request(&req).await {
                                    warn!(error = %e, "MCP write failed; client gone");
                                    break;
                                }
                            }
                            None => {
                                // All client handles dropped. We can
                                // exit cleanly.
                                break;
                            }
                        }
                    }
                    frame = transport.read_frame() => {
                        match frame {
                            Ok(Some(frame)) => {
                                handle_inbound(frame, &pending_actor, &notif_tx_actor, &mut transport).await;
                            }
                            Ok(None) => {
                                warn!("MCP server closed stdout (EOF)");
                                fail_pending(&pending_actor, McpError::Closed).await;
                                break;
                            }
                            Err(e) => {
                                warn!(error = %e, "MCP read failed");
                                fail_pending(&pending_actor, McpError::Closed).await;
                                break;
                            }
                        }
                    }
                }
            }
            let _ = transport.shutdown().await;
        });

        Ok(Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(2), // 0 was init, leave gap
                requests: tx,
                notifs: notif_tx,
                server_capabilities,
            }),
        })
    }

    /// Subscribe to notifications. Each call yields its own receiver.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<McpNotification> {
        self.inner.notifs.subscribe()
    }

    /// Server's advertised capabilities from the initialize handshake.
    pub fn server_capabilities(&self) -> &ServerCapabilities {
        &self.inner.server_capabilities
    }

    pub async fn list_tools(&self) -> McpResult<Vec<McpTool>> {
        let result: ListToolsResult = self.call(methods::TOOLS_LIST, None).await?;
        Ok(result.tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> McpResult<CallToolResult> {
        let params = serde_json::to_value(CallToolParams { name, arguments })?;
        self.call::<CallToolResult>(methods::TOOLS_CALL, Some(params))
            .await
    }

    pub async fn list_resources(&self) -> McpResult<Vec<McpResource>> {
        let result: ListResourcesResult = self.call(methods::RESOURCES_LIST, None).await?;
        Ok(result.resources)
    }

    pub async fn read_resource(&self, uri: &str) -> McpResult<ReadResourceResult> {
        let params = serde_json::to_value(ReadResourceParams { uri })?;
        self.call(methods::RESOURCES_READ, Some(params)).await
    }

    /// Generic request/response. Internal use; public methods above
    /// are typed wrappers.
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Option<Value>) -> McpResult<T> {
        let _ = self.inner.next_id.fetch_add(0, Ordering::SeqCst); // touch to keep `next_id` field used
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .requests
            .send(PendingCall {
                method: method.to_owned(),
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| McpError::ClientGone)?;
        let value = tokio::time::timeout(REQUEST_TIMEOUT, reply_rx)
            .await
            .map_err(|_| McpError::Timeout(REQUEST_TIMEOUT))?
            .map_err(|_| McpError::ClientGone)??;
        let typed: T = serde_json::from_value(value)
            .map_err(|e| McpError::Protocol(format!("decoding {method} result: {e}")))?;
        Ok(typed)
    }
}

impl From<serde_json::Error> for McpError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(format!("serde_json: {e}"))
    }
}

/// Read inbound frames until we find a response matching `expect_id`.
/// Notifications encountered during the wait are forwarded to the
/// notification channel; server-initiated requests are refused.
async fn read_until_response<T: DeserializeOwned>(
    transport: &mut StdioTransport,
    expect_id: &RpcId,
    timeout: Duration,
    notif_tx: &broadcast::Sender<McpNotification>,
) -> McpResult<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(McpError::Timeout(timeout))?;
        let frame = tokio::time::timeout(remaining, transport.read_frame())
            .await
            .map_err(|_| McpError::Timeout(timeout))??
            .ok_or(McpError::Closed)?;
        match (frame.id.as_ref(), frame.method.as_deref()) {
            (Some(id), None) if id == expect_id => {
                if let Some(err) = frame.error {
                    return Err(McpError::Server {
                        code: err.code,
                        message: err.message,
                    });
                }
                let result = frame.result.unwrap_or(Value::Null);
                return serde_json::from_value(result)
                    .map_err(|e| McpError::Protocol(format!("decoding init result: {e}")));
            }
            (Some(id), Some(method)) => {
                // Server-initiated request during the handshake.
                refuse_server_request(transport, id, method).await?;
            }
            (None, Some(method)) => {
                forward_notification(method, notif_tx);
            }
            _ => {
                debug!("ignoring unrecognised frame during handshake");
            }
        }
    }
}

async fn handle_inbound(
    frame: InboundFrame,
    pending: &Arc<PendingMap>,
    notifs: &broadcast::Sender<McpNotification>,
    transport: &mut StdioTransport,
) {
    match (frame.id.as_ref(), frame.method.as_deref()) {
        (Some(id), None) => {
            // Response to one of our requests.
            let waiter = pending.lock().await.remove(id);
            if let Some(tx) = waiter {
                let outcome = match (frame.error, frame.result) {
                    (Some(err), _) => Err(McpError::Server {
                        code: err.code,
                        message: err.message,
                    }),
                    (None, Some(v)) => Ok(v),
                    (None, None) => Ok(Value::Null),
                };
                let _ = tx.send(outcome);
            } else {
                warn!("MCP response for unknown id {id:?}");
            }
        }
        (Some(id), Some(method)) => {
            // Server-initiated request — refuse.
            if let Err(e) = refuse_server_request(transport, id, method).await {
                warn!(error = %e, "failed to send refusal for server request");
            }
        }
        (None, Some(method)) => {
            forward_notification(method, notifs);
        }
        _ => {
            debug!("ignoring unrecognised inbound frame");
        }
    }
}

async fn refuse_server_request(
    transport: &mut StdioTransport,
    id: &RpcId,
    method: &str,
) -> McpResult<()> {
    warn!(
        method,
        "refusing server-initiated MCP request (sampling/createMessage and friends are never granted)"
    );
    // Build a JSON-RPC error response by hand — MCP doesn't provide
    // a typed error response builder.
    let err_response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": error_codes::METHOD_NOT_FOUND,
            "message": format!("execlaw refuses server-initiated '{method}' requests"),
        }
    });
    let mut s = serde_json::to_string(&err_response)
        .map_err(|e| McpError::Protocol(format!("encoding refusal: {e}")))?;
    s.push('\n');
    use tokio::io::AsyncWriteExt;
    transport
        .stdin_mut()
        .write_all(s.as_bytes())
        .await
        .map_err(McpError::Io)?;
    transport.stdin_mut().flush().await.map_err(McpError::Io)
}

fn forward_notification(method: &str, notifs: &broadcast::Sender<McpNotification>) {
    let payload = match method {
        notifications::TOOLS_LIST_CHANGED => Some(McpNotification::ToolsListChanged),
        notifications::RESOURCES_LIST_CHANGED => Some(McpNotification::ResourcesListChanged),
        _ => None,
    };
    if let Some(n) = payload {
        let _ = notifs.send(n);
    }
}

async fn fail_pending(pending: &Arc<PendingMap>, err_template: McpError) {
    let mut map = pending.lock().await;
    let drained: Vec<_> = map.drain().collect();
    drop(map);
    for (_, tx) in drained {
        let e = match &err_template {
            McpError::Closed => McpError::Closed,
            McpError::ClientGone => McpError::ClientGone,
            other => McpError::Protocol(other.to_string()),
        };
        let _ = tx.send(Err(e));
    }
}
