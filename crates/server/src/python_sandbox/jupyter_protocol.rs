//! Jupyter messaging protocol — wire types.
//!
//! Reference: <https://jupyter-client.readthedocs.io/en/stable/messaging.html>
//!
//! We carry only the fields we actually use. A full Jupyter client
//! would also handle: `kernel_info_request`, `complete_request`,
//! `inspect_request`, `is_complete_request`, `comm_*`, `input_request`
//! (raw_input from the kernel), `clear_output`, `update_display_data`.
//! None of these are reached by our `python.execute` flow — we
//! request execution and read back text/result/error/stream.
//!
//! Channels:
//!   - `shell`  — outgoing requests, incoming replies
//!   - `iopub`  — broadcast output (stream/result/display/error/status)
//!   - `stdin`  — kernel asks for user input (we set `allow_stdin=false`)
//!   - `control`— kernel-management requests (interrupt/shutdown — we
//!                use the gateway's HTTP API instead, so don't open this)
//!
//! Phase 1 smoke verified the message shapes here against gateway 3.0.1
//! + ipykernel 6.30.0. If those bump, re-run `smoke_execute.py` before
//! editing these structs.

use serde::{Deserialize, Serialize};

/// Channel a message is routed on. Jupyter sends this as an
/// out-of-band field on the envelope (NOT inside the header, despite
/// what some old docs claim) — our serde repr matches the gateway's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KernelChannel {
    Shell,
    Iopub,
    Stdin,
    Control,
}

/// `header.msg_type` values we care about. Unknown / unhandled types
/// deserialize to [`MsgType::Other`] with the raw string preserved so
/// a log can show "we got `kernel_info_reply` and ignored it"
/// instead of failing the WS read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MsgType {
    Known(KnownMsgType),
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownMsgType {
    // Outgoing
    ExecuteRequest,
    // Incoming — shell
    ExecuteReply,
    // Incoming — iopub
    Status,
    ExecuteInput,
    ExecuteResult,
    DisplayData,
    Stream,
    Error,
    UpdateDisplayData,
    ClearOutput,
}

/// Kernel's coarse busy/idle state. Each execute brackets a busy/idle
/// pair on iopub; we use the trailing `idle` (matched to our request
/// via `parent_header.msg_id`) as the signal that all outputs for
/// our execute have arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionState {
    Busy,
    Idle,
    Starting,
}

/// Message envelope — what each WS frame deserializes to.
///
/// `content` stays `serde_json::Value` because its shape is keyed on
/// `header.msg_type` and de-shaping at the envelope level would force
/// a giant tagged enum that's brittle as new Jupyter types appear.
/// Callers match on `header.msg_type` then deserialize `content`
/// into the specific shape they expect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterEnvelope {
    pub header: JupyterHeader,
    #[serde(default)]
    pub parent_header: Option<JupyterHeader>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub content: serde_json::Value,
    pub channel: KernelChannel,
    /// Binary buffers attached out-of-band. We never produce these
    /// and treat incoming buffers as opaque (kernels rarely send
    /// them via the websocket bridge anyway).
    #[serde(default)]
    pub buffers: Vec<serde_json::Value>,
}

/// Message header. `msg_id` is a UUID we generate per outgoing
/// message; `parent_header.msg_id` on responses must match for us
/// to associate them with our request (the gateway broadcasts iopub
/// to all clients connected to the kernel; if a sibling client is
/// dispatching at the same time we'll see their iopub interleaved
/// with ours).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterHeader {
    pub msg_id: String,
    pub username: String,
    pub session: String,
    pub msg_type: String,
    pub version: String,
    /// ISO-8601. Some kernels send `2026-05-18T02:17:32.720638Z`,
    /// some send `2026-05-18T02:17:32.720638+00:00`; we don't parse
    /// it, just round-trip.
    pub date: String,
}

/// Body of an outgoing `execute_request`. Built by the WS dispatcher
/// from the agent's `python.execute` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequestContent {
    pub code: String,
    pub silent: bool,
    pub store_history: bool,
    pub user_expressions: serde_json::Value,
    pub allow_stdin: bool,
    pub stop_on_error: bool,
}

impl ExecuteRequestContent {
    /// The shape our dispatcher always uses. Constants here pin the
    /// kernel's behavior to what the SPA's UX expects:
    ///   * silent=false  — we WANT the iopub broadcast of outputs
    ///   * store_history=true — `_` and `In[N]` work across turns
    ///   * allow_stdin=false — kernel won't block waiting for input()
    ///   * stop_on_error=true — multi-statement cells abort on first
    ///     exception instead of running subsequent lines
    pub fn for_agent(code: String) -> Self {
        Self {
            code,
            silent: false,
            store_history: true,
            user_expressions: serde_json::json!({}),
            allow_stdin: false,
            stop_on_error: true,
        }
    }
}

/// Body of an incoming `status` iopub message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusContent {
    pub execution_state: ExecutionState,
}

/// Body of an incoming `stream` iopub message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamContent {
    pub name: String,
    pub text: String,
}

/// Body of an incoming `execute_result` iopub message. The `data`
/// field is the MIME bundle — Jupyter packs it as
/// `{ "text/plain": "...", "text/html": "..." }`. Our [`crate::python_sandbox::mime`]
/// module re-shapes it into the wire form we hand to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResultContent {
    pub execution_count: u32,
    pub data: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Body of an incoming `display_data` iopub message. Same shape as
/// execute_result minus the execution_count (display_data can fire
/// independently of cell return values via `display(...)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayDataContent {
    pub data: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Body of an incoming `error` iopub message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorContent {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

/// Body of an incoming `execute_reply` shell message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteReplyContent {
    /// "ok" or "error".
    pub status: String,
    pub execution_count: u32,
}

/// Build an outgoing `execute_request` envelope ready to serialize
/// onto the kernel's channels WebSocket.
///
/// Fresh `msg_id` per call — callers MUST keep this id around and
/// match incoming envelopes against it via `parent_header.msg_id` so
/// they don't pick up iopub broadcasts intended for a sibling client.
/// Session id is also per-call (we don't multiplex multiple requests
/// over a single session because we open one WS per execute, not a
/// long-lived shared connection).
pub fn build_execute_request(
    msg_id: &str,
    session: &str,
    code: &str,
) -> JupyterEnvelope {
    JupyterEnvelope {
        header: JupyterHeader {
            msg_id: msg_id.to_string(),
            username: "execlaw".to_string(),
            session: session.to_string(),
            msg_type: "execute_request".to_string(),
            version: "5.3".to_string(),
            // Jupyter spec allows either `+00:00` or `Z` suffix; we
            // emit `Z` to match what the kernel itself sends.
            date: chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        },
        parent_header: None,
        metadata: serde_json::json!({}),
        content: serde_json::to_value(ExecuteRequestContent::for_agent(code.to_string()))
            .expect("ExecuteRequestContent always serializes"),
        channel: KernelChannel::Shell,
        buffers: Vec::new(),
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Sample envelope captured from the Phase 1 smoke against
    /// gateway 3.0.1 + ipykernel 6.30.0 — `1 + 1` execute_result.
    /// If serde can deserialize this it'll deserialize what the
    /// gateway actually sends.
    const SAMPLE_EXECUTE_RESULT: &str = r#"{
        "header": {
            "msg_id": "abc-123",
            "username": "test",
            "session": "session-uuid",
            "msg_type": "execute_result",
            "version": "5.3",
            "date": "2026-05-18T02:17:32.720638Z"
        },
        "parent_header": {
            "msg_id": "parent-456",
            "username": "test",
            "session": "session-uuid",
            "msg_type": "execute_request",
            "version": "5.3",
            "date": "2026-05-18T02:17:32.500000Z"
        },
        "metadata": {},
        "content": {
            "data": {"text/plain": "2"},
            "metadata": {},
            "execution_count": 1
        },
        "channel": "iopub",
        "buffers": []
    }"#;

    #[test]
    fn envelope_deserializes_real_execute_result() {
        let env: JupyterEnvelope = serde_json::from_str(SAMPLE_EXECUTE_RESULT).unwrap();
        assert_eq!(env.channel, KernelChannel::Iopub);
        assert_eq!(env.header.msg_type, "execute_result");
        assert_eq!(env.parent_header.as_ref().unwrap().msg_id, "parent-456");

        let content: ExecuteResultContent = serde_json::from_value(env.content).unwrap();
        assert_eq!(content.execution_count, 1);
        assert_eq!(
            content.data.get("text/plain"),
            Some(&serde_json::json!("2"))
        );
    }

    #[test]
    fn envelope_with_no_parent_header_is_fine() {
        // First `status: starting` after kernel spawn has no
        // parent_header. Must deserialize without it.
        let env: JupyterEnvelope = serde_json::from_str(
            r#"{
                "header": {
                    "msg_id": "x",
                    "username": "u",
                    "session": "s",
                    "msg_type": "status",
                    "version": "5.3",
                    "date": "2026-05-18T02:17:30.000Z"
                },
                "metadata": {},
                "content": {"execution_state": "starting"},
                "channel": "iopub"
            }"#,
        )
        .unwrap();
        assert!(env.parent_header.is_none());
        let st: StatusContent = serde_json::from_value(env.content).unwrap();
        assert_eq!(st.execution_state, ExecutionState::Starting);
    }

    #[test]
    fn execute_request_for_agent_has_safe_defaults() {
        let c = ExecuteRequestContent::for_agent("1 + 1".into());
        assert!(!c.silent);
        assert!(c.store_history);
        assert!(!c.allow_stdin, "kernel must not block on input()");
        assert!(c.stop_on_error, "abort on first exception");
    }

    #[test]
    fn stream_content_distinguishes_stdout_stderr() {
        let s: StreamContent =
            serde_json::from_str(r#"{"name":"stderr","text":"hello-stderr\n"}"#).unwrap();
        assert_eq!(s.name, "stderr");
        assert_eq!(s.text, "hello-stderr\n");
    }

    #[test]
    fn build_execute_request_round_trips() {
        // Build, serialize, parse — assert the serialized JSON
        // matches what the gateway would accept on the shell channel.
        let env = build_execute_request("msg-abc", "session-xyz", "1 + 1");
        let json = serde_json::to_string(&env).unwrap();
        let back: JupyterEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.header.msg_type, "execute_request");
        assert_eq!(back.header.msg_id, "msg-abc");
        assert_eq!(back.header.session, "session-xyz");
        assert_eq!(back.channel, KernelChannel::Shell);
        assert!(back.parent_header.is_none());

        let c: ExecuteRequestContent = serde_json::from_value(back.content).unwrap();
        assert_eq!(c.code, "1 + 1");
        assert!(!c.silent);
        assert!(!c.allow_stdin);
        assert!(c.stop_on_error);
    }

    #[test]
    fn error_content_preserves_ansi_traceback() {
        // What gateway 3.0.1 actually sends for `1/0`: traceback
        // strings carry real ESC (0x1b) bytes for ANSI color codes.
        // Per JSON spec, control chars 0x00-0x1F MUST be escaped
        // on the wire as `\uXXXX` — the gateway does this
        // correctly, and that's the round-trip path we exercise.
        //
        // Source uses a regular (non-raw) string so `` is the
        // 6-char JSON escape sequence in the literal, NOT an actual
        // ESC byte in this .rs file. serde_json decodes it back to
        // one ESC byte in the resulting Rust String.
        let json = "{\n  \"ename\": \"ZeroDivisionError\",\n  \"evalue\": \"division by zero\",\n  \"traceback\": [\n    \"\\u001b[31m---\\u001b[39m\",\n    \"\\u001b[31mZeroDivisionError\\u001b[39m: division by zero\"\n  ]\n}";
        let e: ErrorContent = serde_json::from_str(json).unwrap();
        assert_eq!(e.ename, "ZeroDivisionError");
        assert_eq!(e.traceback.len(), 2);
        assert!(
            e.traceback[0].contains('\u{1b}'),
            "ANSI ESC byte must round-trip; got {:?}",
            e.traceback[0]
        );
        assert!(e.traceback[0].contains("[31m"));
    }
}
