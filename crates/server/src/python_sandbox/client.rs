//! HTTP client for the kernel gateway sidecar.
//!
//! Phase 2a scope: kernel-lifecycle HTTP only — `create`, `list`,
//! `get`, `delete`, `interrupt`, `restart`. The WS `execute` path
//! lands in Phase 2b on top of this client.
//!
//! Design:
//!   - One `GatewayClient` per process. Cheap to clone (`Arc` inside
//!     the `reqwest::Client`).
//!   - Base URL comes from the supervisor at construction time
//!     (the sidecar's mapped 127.0.0.1:<pool_port>). Stable across
//!     the supervisor's lifetime; if the supervisor restarts the
//!     container, port stays pinned.
//!   - Every request has a generous-but-bounded timeout. Default 10s
//!     covers kernel-spawn cold-start (~600 ms in Phase 1 bench)
//!     with headroom for backpressure / OS scheduler hiccups.
//!   - Errors carry actionable context. The gateway returns
//!     `{"reason":"…","message":"…"}` on 4xx; we surface that text.

use crate::python_sandbox::jupyter_protocol::{
    self, DisplayDataContent, ErrorContent, ExecuteReplyContent, ExecuteResultContent,
    ExecutionState, JupyterEnvelope, StatusContent, StreamContent,
};
use crate::python_sandbox::mime::{
    ExecuteOutput, ExecuteResult, ExecuteStatus, StreamName, mime_bundle_from_jupyter_data,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Default per-cell execute deadline if the caller doesn't override.
/// Matches the `python.execute` schema's `timeout_ms.default` (30 s).
pub const DEFAULT_EXECUTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard ceiling on accumulated output bytes per execute call.
///
/// At 50 MB we still cover legitimate analyst outputs (large
/// DataFrame `_repr_html_`, a 5K-row CSV preview, a 4K PNG plot
/// at ~2 MB) while preventing an adversarial cell that prints
/// gigabytes from OOM'ing the host. When the counter trips the
/// kernel is interrupted, a synthetic `OutputTooLarge` error is
/// appended, and the call returns with `status: OutputTooLarge`.
///
/// Picked over a per-output cap because the failure modes we care
/// about (loops that print, infinite recursions, accidentally
/// `print(df_with_10M_rows)`) all manifest as MANY medium outputs
/// rather than one giant one — a per-output cap wouldn't catch them.
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024 * 1024;

/// Strongly-typed kernel id. Wraps the gateway's UUID-as-string.
/// Distinct from `ConversationId`, `AttachmentId`, etc. so the
/// compiler catches an accidental swap at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KernelId(pub String);

impl KernelId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KernelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the gateway returns for a kernel — both as the body of
/// `POST /api/kernels` and as elements of `GET /api/kernels`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelInfo {
    pub id: KernelId,
    pub name: String,
    pub last_activity: String,
    pub execution_state: String,
    pub connections: u32,
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("gateway request failed: {0}")]
    Transport(#[from] reqwest::Error),

    /// Gateway returned a 4xx/5xx with a structured body. The
    /// `reason`/`message` pair comes straight from the gateway and
    /// is generally human-readable (`"reason":"Forbidden"`,
    /// `"reason":"Not Found"`, etc.).
    #[error("gateway returned {status}: {reason}: {message}")]
    Status {
        status: u16,
        reason: String,
        message: String,
    },

    /// Gateway returned a non-2xx with a body we couldn't parse as
    /// the structured error envelope. We keep the raw body for
    /// debugging — historically this catches mismatches when an
    /// upstream proxy strips JSON.
    #[error("gateway returned {status} with non-JSON body: {body}")]
    UnstructuredStatus { status: u16, body: String },

    /// Successful response, but its body didn't deserialize into the
    /// shape we expected. Forward-compat hint: gateway's API
    /// changed under us.
    #[error("gateway response shape unexpected: {0}")]
    Shape(String),

    /// WebSocket transport error — connection failure, broken frame,
    /// unexpected close. Separate variant from `Transport` (which is
    /// HTTP only) so callers can distinguish "couldn't reach gateway
    /// at all" from "execute pipe broke mid-flight."
    #[error("gateway websocket error: {0}")]
    Ws(String),
}

#[derive(Deserialize)]
struct GatewayErrorBody {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Client to a single kernel-gateway sidecar.
#[derive(Clone)]
pub struct GatewayClient {
    inner: Arc<GatewayClientInner>,
}

struct GatewayClientInner {
    http: reqwest::Client,
    /// Root URL — e.g. `http://127.0.0.1:8501`. No trailing slash.
    base: String,
}

impl GatewayClient {
    /// Construct a client against the supervisor-published port.
    pub fn new(base_url: impl Into<String>) -> Result<Self, GatewayError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()?;
        let base = trim_trailing_slash(base_url.into());
        Ok(Self {
            inner: Arc::new(GatewayClientInner { http, base }),
        })
    }

    /// Construct with a pre-configured `reqwest::Client`. Tests use
    /// this to swap in a client wired to `wiremock`.
    pub fn with_http(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        let base = trim_trailing_slash(base_url.into());
        Self {
            inner: Arc::new(GatewayClientInner { http, base }),
        }
    }

    /// Liveness probe — `GET /api/kernels`. Returns `Ok(true)` iff
    /// the gateway is reachable and responding 2xx. The supervisor
    /// uses this; the `python_sandbox` module uses it before
    /// dialing for an execute to fail fast on gateway crashes.
    pub async fn is_healthy(&self) -> bool {
        match self.list_kernels().await {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    /// `POST /api/kernels` — spawn a kernel. `name` must match a
    /// kernelspec the gateway knows about. We always use `"python3"`
    /// (the only spec the `python-sandbox:fast` image ships).
    pub async fn create_kernel(&self, name: &str) -> Result<KernelInfo, GatewayError> {
        let url = format!("{}/api/kernels", self.inner.base);
        let resp = self
            .inner
            .http
            .post(&url)
            .json(&serde_json::json!({"name": name}))
            .send()
            .await?;
        decode_json(resp).await
    }

    /// `GET /api/kernels` — list all kernels the gateway knows
    /// about. Used for state reconciliation after a host restart.
    pub async fn list_kernels(&self) -> Result<Vec<KernelInfo>, GatewayError> {
        let url = format!("{}/api/kernels", self.inner.base);
        let resp = self.inner.http.get(&url).send().await?;
        decode_json(resp).await
    }

    /// `GET /api/kernels/<id>` — fetch one kernel's status.
    /// `Ok(None)` on 404 (kernel was culled / never existed).
    pub async fn get_kernel(&self, id: &KernelId) -> Result<Option<KernelInfo>, GatewayError> {
        let url = format!("{}/api/kernels/{}", self.inner.base, id.as_str());
        let resp = self.inner.http.get(&url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        decode_json(resp).await.map(Some)
    }

    /// `DELETE /api/kernels/<id>` — shut a kernel down. Idempotent:
    /// a 404 means "already gone" and is treated as success so the
    /// caller's idle-evict loop doesn't have to special-case races.
    pub async fn delete_kernel(&self, id: &KernelId) -> Result<(), GatewayError> {
        let url = format!("{}/api/kernels/{}", self.inner.base, id.as_str());
        let resp = self.inner.http.delete(&url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        decode_empty(resp).await
    }

    /// `POST /api/kernels/<id>/interrupt` — send the equivalent of
    /// `Ctrl+C` to the kernel's executing cell. Returns immediately;
    /// the kernel's iopub stream is the source of truth for "did the
    /// running cell actually stop." Idempotent on a not-running
    /// kernel.
    pub async fn interrupt_kernel(&self, id: &KernelId) -> Result<(), GatewayError> {
        let url = format!("{}/api/kernels/{}/interrupt", self.inner.base, id.as_str());
        let resp = self.inner.http.post(&url).send().await?;
        decode_empty(resp).await
    }

    /// `POST /api/kernels/<id>/restart` — hard reset. Kills the
    /// kernel subprocess and spawns a fresh one with the SAME id.
    /// This is what `python.reset` is wired to: cheaper than
    /// delete + create, and the id stability matters because our
    /// per-conversation map is keyed on it.
    pub async fn restart_kernel(&self, id: &KernelId) -> Result<KernelInfo, GatewayError> {
        let url = format!("{}/api/kernels/{}/restart", self.inner.base, id.as_str());
        let resp = self.inner.http.post(&url).send().await?;
        decode_json(resp).await
    }

    /// Run one `execute_request` against the kernel, draining iopub
    /// and shell until both `execute_reply` and `status: idle` for
    /// our msg_id have arrived. Builds and returns the full
    /// [`ExecuteResult`] including MIME-bundle outputs.
    ///
    /// Behavior on edge cases:
    ///   * **deadline tripped** — sends an HTTP interrupt to the kernel
    ///     and returns the partial outputs collected so far with
    ///     `status: Timeout`. Caller decides whether to surface or
    ///     retry.
    ///   * **WS closes mid-execute** — kernel subprocess crashed or
    ///     gateway dropped us; returns partial outputs with
    ///     `status: KernelDied`.
    ///   * **stray iopub from a sibling client** — gateway broadcasts
    ///     all iopub to every connected client; we filter by
    ///     `parent_header.msg_id` matching our request.
    ///   * **unparseable envelope** — log + skip rather than abort.
    ///     The gateway has occasionally bumped its message schema
    ///     between versions; one stray field shouldn't fail the
    ///     whole execute.
    pub async fn execute(
        &self,
        kernel: &KernelId,
        code: &str,
        timeout: Duration,
    ) -> Result<ExecuteResult, GatewayError> {
        let start = Instant::now();
        let msg_id = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        let request = jupyter_protocol::build_execute_request(&msg_id, &session, code);
        let ws_url = self.ws_channels_url(kernel);
        let deadline = tokio::time::Instant::now() + timeout;

        tracing::debug!(%kernel, msg_id = %msg_id, "python_sandbox execute starting");

        // Open the WS, bounded by the same deadline so a slow gateway
        // can't extend the caller's timeout budget unilaterally.
        //
        // We raise tokio_tungstenite's per-frame/message ceilings to
        // 128 MiB — comfortably above our MAX_OUTPUT_BYTES (50 MiB)
        // so a single big iopub message (e.g. a 50 MB DataFrame HTML
        // repr) reaches our cap check rather than triggering an
        // opaque `Space limit exceeded` WS error. Above 128 MiB the
        // WS layer hard-rejects; that's still a graceful failure
        // (caller sees GatewayError::Ws) but our cap-fires-first
        // path is the preferred outcome.
        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
            max_message_size: Some(128 * 1024 * 1024),
            max_frame_size: Some(128 * 1024 * 1024),
            ..Default::default()
        };
        let connect_fut =
            tokio_tungstenite::connect_async_with_config(&ws_url, Some(ws_config), false);
        let (ws_stream, _) = match tokio::time::timeout_at(deadline, connect_fut).await {
            Err(_) => return Ok(timeout_result(Vec::new(), 0, start)),
            Ok(Err(e)) => return Err(GatewayError::Ws(e.to_string())),
            Ok(Ok(p)) => p,
        };
        let (mut sink, mut stream) = ws_stream.split();

        // Send execute_request.
        let body = serde_json::to_string(&request)
            .map_err(|e| GatewayError::Shape(format!("serialize execute_request: {e}")))?;
        sink.send(Message::Text(body.into()))
            .await
            .map_err(|e| GatewayError::Ws(format!("ws send: {e}")))?;

        let mut outputs: Vec<ExecuteOutput> = Vec::new();
        let mut execution_count: u32 = 0;
        let mut reply_status: Option<String> = None;
        let mut got_idle = false;
        let mut got_reply = false;
        // Running output-bytes counter for the OutputTooLarge guard.
        // Incremented inside handle_envelope via the byte-aware
        // variant; we check after each iopub message.
        let mut output_bytes: usize = 0;

        loop {
            // Both completion signals received? We're done.
            if got_reply && got_idle {
                break;
            }
            let recv = tokio::time::timeout_at(deadline, stream.next()).await;
            match recv {
                Err(_) => {
                    // Deadline tripped. Send an interrupt so the
                    // kernel actually stops; otherwise the cell
                    // keeps running and we'd have to handle late
                    // iopub on the next execute.
                    if let Err(e) = self.interrupt_kernel(kernel).await {
                        tracing::warn!(?e, %kernel, "interrupt after timeout failed");
                    }
                    return Ok(timeout_result(outputs, execution_count, start));
                }
                Ok(None) => {
                    tracing::warn!(%kernel, "ws stream ended mid-execute (kernel died?)");
                    return Ok(ExecuteResult {
                        outputs,
                        execution_count,
                        duration_ms: start.elapsed().as_millis() as u64,
                        status: ExecuteStatus::KernelDied,
                        created_files: None,
                    });
                }
                Ok(Some(Err(e))) => return Err(GatewayError::Ws(format!("ws recv: {e}"))),
                Ok(Some(Ok(msg))) => {
                    let text = match msg {
                        Message::Text(t) => t.to_string(),
                        Message::Close(_) => break,
                        // Ping/pong/binary/frame — not our concern.
                        _ => continue,
                    };
                    let env: JupyterEnvelope = match serde_json::from_str(&text) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!(?e, "unparseable jupyter envelope, skipping");
                            continue;
                        }
                    };
                    // Filter: gateway broadcasts iopub to every connected
                    // client. If a sibling client is dispatching, we'll
                    // see their messages too — drop them.
                    let parent_matches = env
                        .parent_header
                        .as_ref()
                        .map(|h| h.msg_id == msg_id)
                        .unwrap_or(false);
                    if !parent_matches {
                        continue;
                    }
                    let outputs_before = outputs.len();
                    handle_envelope(
                        env,
                        &mut outputs,
                        &mut execution_count,
                        &mut reply_status,
                        &mut got_idle,
                        &mut got_reply,
                    );
                    // Tally bytes from anything new the envelope
                    // appended. handle_envelope is the ONLY thing
                    // that mutates `outputs`, so this delta is
                    // exact and we never recount old entries.
                    for new in &outputs[outputs_before..] {
                        output_bytes = output_bytes.saturating_add(new.approx_bytes());
                    }
                    if output_bytes > MAX_OUTPUT_BYTES {
                        tracing::warn!(
                            %kernel,
                            output_bytes,
                            limit = MAX_OUTPUT_BYTES,
                            "python_sandbox cell exceeded output cap; interrupting"
                        );
                        if let Err(e) = self.interrupt_kernel(kernel).await {
                            tracing::warn!(
                                ?e, %kernel,
                                "interrupt after OutputTooLarge failed"
                            );
                        }
                        // Append a synthetic terminal error so the
                        // agent / SPA gets a clear "this is why I
                        // stopped" line of context. Not a Python
                        // exception in the kernel; ours.
                        outputs.push(ExecuteOutput::Error {
                            ename: "OutputTooLarge".into(),
                            evalue: format!(
                                "Cell output exceeded {} MB; the kernel was interrupted. \
                                 Trim print()s or use .head() on large DataFrames.",
                                MAX_OUTPUT_BYTES / 1024 / 1024
                            ),
                            traceback: Vec::new(),
                        });
                        return Ok(ExecuteResult {
                            outputs,
                            execution_count,
                            duration_ms: start.elapsed().as_millis() as u64,
                            status: ExecuteStatus::OutputTooLarge,
                            created_files: None,
                        });
                    }
                }
            }
        }

        let status = match reply_status.as_deref() {
            Some("error") => ExecuteStatus::Error,
            _ => ExecuteStatus::Ok,
        };

        let elapsed = start.elapsed().as_millis() as u64;
        tracing::debug!(%kernel, elapsed_ms = elapsed, ?status, "python_sandbox execute done");

        Ok(ExecuteResult {
            outputs,
            execution_count,
            duration_ms: elapsed,
            status,
            created_files: None,
        })
    }

    /// Build the WebSocket URL for a kernel's messaging channel.
    /// Phase 2b uses this when opening the execute pipe; placed
    /// here in 2a because the host scheme transform (`http://` →
    /// `ws://`) is a base-URL concern, not a per-execute concern.
    pub fn ws_channels_url(&self, id: &KernelId) -> String {
        let ws_base = if let Some(rest) = self.inner.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else if let Some(rest) = self.inner.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            // No scheme? Default to ws (we constructed `base` from a
            // supervisor-published URL, which is always
            // `http://127.0.0.1:<port>`, so this branch is
            // defensive).
            format!("ws://{}", self.inner.base)
        };
        format!("{ws_base}/api/kernels/{}/channels", id.as_str())
    }
}

/// Build a Timeout-tagged [`ExecuteResult`] preserving any outputs
/// we already collected. Used by `execute` when the deadline trips
/// either pre-connect or mid-stream.
fn timeout_result(
    outputs: Vec<ExecuteOutput>,
    execution_count: u32,
    start: Instant,
) -> ExecuteResult {
    ExecuteResult {
        outputs,
        execution_count,
        duration_ms: start.elapsed().as_millis() as u64,
        status: ExecuteStatus::Timeout,
        created_files: None,
    }
}

/// Dispatch one matched-by-parent-header envelope into the
/// accumulating `ExecuteResult` state. Pure function over its
/// arguments — easy to read, easy to test without a WS.
fn handle_envelope(
    env: JupyterEnvelope,
    outputs: &mut Vec<ExecuteOutput>,
    execution_count: &mut u32,
    reply_status: &mut Option<String>,
    got_idle: &mut bool,
    got_reply: &mut bool,
) {
    let mt = env.header.msg_type.as_str();
    match mt {
        "status" => {
            if let Ok(s) = serde_json::from_value::<StatusContent>(env.content) {
                if matches!(s.execution_state, ExecutionState::Idle) {
                    *got_idle = true;
                }
            }
        }
        "execute_input" => { /* bookkeeping, ignore */ }
        "execute_result" => {
            if let Ok(c) = serde_json::from_value::<ExecuteResultContent>(env.content) {
                *execution_count = c.execution_count;
                outputs.push(ExecuteOutput::ExecuteResult {
                    execution_count: c.execution_count,
                    bundle: mime_bundle_from_jupyter_data(&c.data),
                });
            }
        }
        "display_data" => {
            if let Ok(c) = serde_json::from_value::<DisplayDataContent>(env.content) {
                outputs.push(ExecuteOutput::DisplayData {
                    bundle: mime_bundle_from_jupyter_data(&c.data),
                });
            }
        }
        "stream" => {
            if let Ok(c) = serde_json::from_value::<StreamContent>(env.content) {
                let name = match c.name.as_str() {
                    "stdout" => StreamName::Stdout,
                    "stderr" => StreamName::Stderr,
                    // Unknown stream name — drop. Jupyter spec lists
                    // only stdout/stderr; anything else is a kernel
                    // bug and shouldn't pollute outputs.
                    _ => return,
                };
                outputs.push(ExecuteOutput::Stream { name, text: c.text });
            }
        }
        "error" => {
            if let Ok(c) = serde_json::from_value::<ErrorContent>(env.content) {
                outputs.push(ExecuteOutput::Error {
                    ename: c.ename,
                    evalue: c.evalue,
                    traceback: c.traceback,
                });
            }
        }
        "execute_reply" => {
            if let Ok(c) = serde_json::from_value::<ExecuteReplyContent>(env.content) {
                *execution_count = c.execution_count;
                *reply_status = Some(c.status);
                *got_reply = true;
            }
        }
        // Unknown msg_types are not errors — Jupyter occasionally
        // adds new ones (kernel_info_reply, comm_msg, etc.). Skip.
        _ => {}
    }
}

fn trim_trailing_slash(mut s: String) -> String {
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// Read a response body that we expect to be JSON of type `T` and
/// returns the parsed value. Maps non-2xx statuses into
/// [`GatewayError::Status`] / [`GatewayError::UnstructuredStatus`].
async fn decode_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, GatewayError> {
    let status = resp.status();
    if status.is_success() {
        let value: T = resp
            .json()
            .await
            .map_err(|e| GatewayError::Shape(e.to_string()))?;
        return Ok(value);
    }
    Err(parse_error(status, resp.text().await.unwrap_or_default()))
}

/// Read a response body that we expect to be empty on success
/// (DELETE / interrupt). Maps non-2xx the same as `decode_json`.
async fn decode_empty(resp: reqwest::Response) -> Result<(), GatewayError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    Err(parse_error(status, resp.text().await.unwrap_or_default()))
}

fn parse_error(status: reqwest::StatusCode, body: String) -> GatewayError {
    match serde_json::from_str::<GatewayErrorBody>(&body) {
        Ok(envelope) if envelope.reason.is_some() || envelope.message.is_some() => {
            GatewayError::Status {
                status: status.as_u16(),
                reason: envelope.reason.unwrap_or_default(),
                message: envelope.message.unwrap_or_default(),
            }
        }
        _ => GatewayError::UnstructuredStatus {
            status: status.as_u16(),
            body,
        },
    }
}

// ===================================================================
// Tests — no live gateway needed; uses wiremock to assert wire shape.
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_transforms_http_to_ws() {
        let c = GatewayClient::new("http://127.0.0.1:8501").unwrap();
        let url = c.ws_channels_url(&KernelId("abc-123".into()));
        assert_eq!(url, "ws://127.0.0.1:8501/api/kernels/abc-123/channels");
    }

    #[test]
    fn ws_url_transforms_https_to_wss() {
        let c = GatewayClient::new("https://gateway.example.com:9443").unwrap();
        let url = c.ws_channels_url(&KernelId("abc".into()));
        assert_eq!(
            url,
            "wss://gateway.example.com:9443/api/kernels/abc/channels"
        );
    }

    #[test]
    fn ws_url_trims_trailing_slashes_on_base() {
        let c = GatewayClient::new("http://127.0.0.1:8501///").unwrap();
        let url = c.ws_channels_url(&KernelId("a".into()));
        assert_eq!(url, "ws://127.0.0.1:8501/api/kernels/a/channels");
    }

    #[test]
    fn kernel_id_round_trip_through_json() {
        let kid = KernelId("ba2a035b-4586-4d9e-b4ba-fb96eafed6e1".into());
        let s = serde_json::to_string(&kid).unwrap();
        assert_eq!(s, "\"ba2a035b-4586-4d9e-b4ba-fb96eafed6e1\"");
        let back: KernelId = serde_json::from_str(&s).unwrap();
        assert_eq!(kid, back);
    }

    #[test]
    fn parse_error_extracts_gateway_envelope() {
        let err = parse_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"reason":"Forbidden","message":"Forbidden"}"#.into(),
        );
        match err {
            GatewayError::Status {
                status,
                reason,
                message,
            } => {
                assert_eq!(status, 403);
                assert_eq!(reason, "Forbidden");
                assert_eq!(message, "Forbidden");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_falls_back_for_html_body() {
        let err = parse_error(
            reqwest::StatusCode::BAD_GATEWAY,
            "<html><body>nginx 502</body></html>".into(),
        );
        match err {
            GatewayError::UnstructuredStatus { status, body } => {
                assert_eq!(status, 502);
                assert!(body.contains("nginx"));
            }
            other => panic!("expected UnstructuredStatus, got {other:?}"),
        }
    }

    /// Live integration: parity with `docker/python-sandbox/smoke_execute.py`.
    /// Executes the same four scenarios through the Rust client and
    /// asserts the exact ExecuteResult shape.
    ///
    /// Gated on EXECLAW_TEST_GATEWAY_URL so `cargo test` doesn't
    /// require Docker.
    #[tokio::test]
    async fn live_execute_parity_with_python_smoke() {
        let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
            eprintln!(
                "skipping: set EXECLAW_TEST_GATEWAY_URL=http://127.0.0.1:18888 \
                 with the image running to exercise this"
            );
            return;
        };
        let c = GatewayClient::new(url).expect("client builds");
        let kernel = c.create_kernel("python3").await.expect("spawn kernel").id;

        // Scenario 1: `1 + 1` → execute_result with text/plain "2".
        let r = c
            .execute(&kernel, "1 + 1", Duration::from_secs(10))
            .await
            .expect("execute 1+1");
        assert!(matches!(r.status, ExecuteStatus::Ok));
        let er = r
            .outputs
            .iter()
            .find_map(|o| match o {
                ExecuteOutput::ExecuteResult { bundle, .. } => Some(bundle),
                _ => None,
            })
            .expect("execute_result expected for `1+1`");
        let plain = er
            .iter()
            .find(|m| m.mime_type == "text/plain")
            .expect("text/plain present");
        assert_eq!(plain.data, serde_json::json!("2"));
        assert_eq!(r.execution_count, 1);

        // Scenario 2: pandas DataFrame → MIME bundle with text/html + text/plain.
        let r = c
            .execute(
                &kernel,
                "import pandas as pd; pd.DataFrame({'a':[1,2],'b':[3,4]})",
                Duration::from_secs(10),
            )
            .await
            .expect("execute DataFrame");
        assert!(matches!(r.status, ExecuteStatus::Ok));
        let bundle = r
            .outputs
            .iter()
            .find_map(|o| match o {
                ExecuteOutput::ExecuteResult { bundle, .. } => Some(bundle),
                _ => None,
            })
            .expect("execute_result for DataFrame");
        let mimes: Vec<&str> = bundle.iter().map(|m| m.mime_type.as_str()).collect();
        assert!(
            mimes.contains(&"text/html"),
            "DataFrame must produce text/html; got {mimes:?}"
        );
        assert!(
            mimes.contains(&"text/plain"),
            "DataFrame must also produce text/plain; got {mimes:?}"
        );

        // Scenario 3: stderr stream.
        let r = c
            .execute(
                &kernel,
                "import sys; print('hello-stderr', file=sys.stderr)",
                Duration::from_secs(10),
            )
            .await
            .expect("execute stderr");
        assert!(matches!(r.status, ExecuteStatus::Ok));
        let stream = r
            .outputs
            .iter()
            .find_map(|o| match o {
                ExecuteOutput::Stream {
                    name: StreamName::Stderr,
                    text,
                } => Some(text),
                _ => None,
            })
            .expect("stderr stream output expected");
        assert!(
            stream.contains("hello-stderr"),
            "stderr text mismatch: {stream:?}"
        );

        // Scenario 4: ZeroDivisionError → status=Error + Error output.
        let r = c
            .execute(&kernel, "1 / 0", Duration::from_secs(10))
            .await
            .expect("execute 1/0");
        assert!(
            matches!(r.status, ExecuteStatus::Error),
            "1/0 must report status=Error, got {:?}",
            r.status
        );
        let err = r
            .outputs
            .iter()
            .find_map(|o| match o {
                ExecuteOutput::Error {
                    ename,
                    evalue,
                    traceback,
                } => Some((ename, evalue, traceback)),
                _ => None,
            })
            .expect("Error output expected");
        assert_eq!(err.0, "ZeroDivisionError");
        assert_eq!(err.1, "division by zero");
        assert!(!err.2.is_empty(), "traceback must not be empty");
        // ANSI ESC byte should round-trip through our wire layer.
        assert!(
            err.2.iter().any(|line| line.contains('\u{1b}')),
            "traceback should preserve ANSI ESC bytes; got {:?}",
            err.2
        );

        // Persistence check — variables defined earlier should still
        // exist (the whole value prop of the kernel pool).
        let r = c
            .execute(&kernel, "pd.__version__", Duration::from_secs(10))
            .await
            .expect("persistence check");
        assert!(
            matches!(r.status, ExecuteStatus::Ok),
            "kernel must remember `pd` import from scenario 2"
        );

        c.delete_kernel(&kernel).await.expect("cleanup");
    }

    /// Live integration: a cell that floods stdout past the
    /// `MAX_OUTPUT_BYTES` ceiling must trip the guard, interrupt
    /// the kernel, and return `status: OutputTooLarge`. The
    /// follow-up execute must succeed — proving the interrupt
    /// actually cleared the cell rather than just bailing client-
    /// side while the kernel kept dumping into iopub.
    #[tokio::test]
    async fn live_execute_output_too_large_is_handled_cleanly() {
        let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
            return;
        };
        let c = GatewayClient::new(url).expect("client");
        let kernel = c.create_kernel("python3").await.expect("spawn").id;

        // 60 MB > 50 MB cap. ipykernel chunks large prints into many
        // stream messages, so the running counter trips somewhere
        // inside the flood.
        let r = c
            .execute(&kernel, "print('x' * 60_000_000)", Duration::from_secs(30))
            .await
            .expect("execute should not error");
        assert!(
            matches!(r.status, ExecuteStatus::OutputTooLarge),
            "expected OutputTooLarge, got {:?} after {} outputs",
            r.status,
            r.outputs.len()
        );
        // Last output is the synthetic terminal error.
        match r.outputs.last() {
            Some(ExecuteOutput::Error { ename, .. }) => {
                assert_eq!(ename, "OutputTooLarge");
            }
            other => panic!("last output should be OutputTooLarge Error, got {other:?}"),
        }

        // Kernel must still be usable.
        let r2 = c
            .execute(&kernel, "1 + 2", Duration::from_secs(10))
            .await
            .expect("post-OutputTooLarge execute");
        assert!(
            matches!(r2.status, ExecuteStatus::Ok),
            "kernel should be usable after OutputTooLarge interrupt; got {:?}",
            r2.status
        );

        c.delete_kernel(&kernel).await.expect("cleanup");
    }

    /// Live integration: a sleeping cell longer than the deadline
    /// returns `status: Timeout` and the kernel is interrupted so
    /// the next execute can proceed without seeing stale iopub.
    #[tokio::test]
    async fn live_execute_timeout_is_handled_cleanly() {
        let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
            return;
        };
        let c = GatewayClient::new(url).expect("client builds");
        let kernel = c.create_kernel("python3").await.expect("spawn").id;

        let r = c
            .execute(
                &kernel,
                "import time; time.sleep(5)",
                Duration::from_millis(300),
            )
            .await
            .expect("execute with short timeout");
        assert!(
            matches!(r.status, ExecuteStatus::Timeout),
            "expected Timeout, got {:?}",
            r.status
        );

        // Next execute must succeed — proves the interrupt cleared
        // the previous cell.
        let r2 = c
            .execute(&kernel, "1 + 2", Duration::from_secs(10))
            .await
            .expect("post-timeout execute");
        assert!(matches!(r2.status, ExecuteStatus::Ok));

        c.delete_kernel(&kernel).await.expect("cleanup");
    }

    /// Live integration: kernel lifecycle (create/list/restart/interrupt/delete).
    /// Carried forward from Phase 2a — kept as the smallest sanity probe.
    #[tokio::test]
    async fn live_gateway_create_list_delete() {
        let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
            eprintln!(
                "skipping: set EXECLAW_TEST_GATEWAY_URL=http://127.0.0.1:18888 \
                 with the image running to exercise this"
            );
            return;
        };
        let c = GatewayClient::new(url).expect("client builds");
        assert!(c.is_healthy().await, "gateway must be healthy");

        // Track OUR kernel by id, not the global count — the live
        // suite runs tests in parallel and they share this gateway,
        // so any count-delta assertion is flaky. Initial fix was
        // `count_before+1 == count_after`; that broke once we added
        // sibling live tests (live_execute_parity, live_pool_lifecycle)
        // running concurrently. Lesson: assert on identity, not on
        // population-level invariants, when the population is shared.
        let info = c.create_kernel("python3").await.unwrap();
        assert_eq!(info.name, "python3");

        let listed = c.list_kernels().await.unwrap();
        assert!(
            listed.iter().any(|k| k.id == info.id),
            "our kernel must appear in the list; got ids {:?}",
            listed.iter().map(|k| &k.id).collect::<Vec<_>>()
        );

        // Idempotent restart preserves the id.
        let restarted = c.restart_kernel(&info.id).await.unwrap();
        assert_eq!(restarted.id, info.id, "restart must keep kernel_id stable");

        // Interrupt on a currently-idle kernel is a no-op but must
        // not error — common in our flow because the SPA's stop
        // button can race with execute completion.
        c.interrupt_kernel(&info.id).await.unwrap();

        // Delete is idempotent — calling twice should not 5xx.
        c.delete_kernel(&info.id).await.unwrap();
        c.delete_kernel(&info.id).await.unwrap();

        let final_list = c.list_kernels().await.unwrap();
        assert!(
            !final_list.iter().any(|k| k.id == info.id),
            "our kernel must be absent after delete"
        );
    }
}
