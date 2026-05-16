//! Wire types shared between the control plane and runner containers.
//!
//! This crate is **transport-agnostic**: it defines the message
//! envelopes that flow over the runner ↔ supervisor WebSocket but
//! says nothing about how to send them. The server side serialises
//! `ServerToRunner` and frames it; the runner deserialises and acts.
//! Same in reverse for `RunnerToServer`.
//!
//! Versioning: every envelope carries a `protocol_version` constant
//! (set by `current_version()`). Runner and supervisor compare on
//! registration; mismatched versions refuse to connect rather than
//! silently miswire.

#![forbid(unsafe_code)]

use execlaw_inference_api::{ChatMessage, ToolDeclaration};
use serde::{Deserialize, Serialize};

/// Bumped whenever the wire protocol changes incompatibly. Both
/// sides verify on registration. Bump rules:
///   * Adding a new variant to `ServerToRunner` / `RunnerToServer`
///     with a default-handling fallback on the receiver: minor (no
///     bump needed if old runners ignore unknown variants).
///   * Renaming or removing a field: MAJOR (bump).
///   * Changing a field's semantics: MAJOR (bump).
///
/// 2026-04-28 — bumped 1 → 2 because `TurnRequest::capability_token`
/// was removed. Old (v1) runner-binary images decode TurnRequest
/// with the missing field as a hard error mid-turn ("decoding
/// ServerToRunner frame"); the version check below catches the
/// mismatch at handshake time and surfaces a clear error in the
/// supervisor's spawn log instead. If you ship a new runner image,
/// rebuild `execlaw/runner:dev` from the matching source tree.
pub const PROTOCOL_VERSION: u32 = 2;

pub fn current_version() -> u32 {
    PROTOCOL_VERSION
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Sent by the runner over the WS upgrade as the value of the
/// `Authorization: Bearer <hex>` header. The supervisor minted this
/// per spawn and stores the expected value in its in-memory
/// `pending_spawns` map; constant-time-compare on receipt.
pub type SpawnSecretHex = String;

/// Body of the supervisor's response to a successful registration —
/// sent as the FIRST frame after the WS upgrade so the runner can
/// confirm the protocol matches and know its assigned identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistrationAck {
    pub protocol_version: u32,
    pub group_id: String,
    /// Server's monotonic clock (ms since unix epoch) at the moment
    /// the registration completed. The runner uses this to seed its
    /// own clock-drift tolerance window for any time-sensitive
    /// frames (e.g. heartbeats).
    pub server_time_ms: i64,
}

// ---------------------------------------------------------------------------
// Server → Runner
// ---------------------------------------------------------------------------

/// Frames the supervisor pushes to a runner. Tagged enum so JSON
/// payloads decode unambiguously.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerToRunner {
    /// Run one turn. The runner streams progress back via
    /// `RunnerToServer` frames keyed on the same `turn_id`. Boxed
    /// because `TurnRequest` carries 300+ bytes of context every
    /// other variant doesn't — without indirection the enum
    /// inflates every channel + Vec it passes through. Heap
    /// allocation cost per Turn frame is negligible vs the JSON
    /// encode + IPC the supervisor performs anyway.
    Turn(Box<TurnRequest>),

    /// Cancel an in-flight turn. The runner aborts its current
    /// model+tool loop and emits a final `Error { reason:
    /// "cancelled" }` for that `turn_id`. The runner stays alive.
    CancelTurn { turn_id: String },

    /// Reply to a tool call the runner issued. `call_id` matches a
    /// prior `RunnerToServer::ToolCallRequest`.
    ToolCallResult(ToolCallResult),

    /// Graceful shutdown. The runner finishes any in-flight turn
    /// (subject to the supervisor's max-turn watchdog) and exits
    /// the process. Workspace volume preservation/wiping happens on
    /// the supervisor side.
    Shutdown { reason: ShutdownReason },

    /// Liveness probe. The runner replies with
    /// `RunnerToServer::HeartbeatAck` carrying the same `nonce`.
    Heartbeat { nonce: u64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    /// Idle TTL hit. Workspace will be wiped after the runner exits.
    IdleReap,
    /// Operator clicked Restart in admin UI. Workspace preserved.
    OperatorRestart,
    /// Operator clicked Wipe Workspace. Volume removed after exit.
    OperatorWipe,
    /// Server is shutting down; runner should exit cleanly.
    ServerShutdown,
    /// Group was deleted from the DB. Volume removed.
    GroupDeleted,
}

/// Everything the runner needs to execute one turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRequest {
    pub turn_id: String,
    /// SPA-visible thread id. The runner uses this for two things:
    ///   1. Filtering the event-log replay (only events for THIS
    ///      thread, not other threads in the same group).
    ///   2. Echoing it back on every `RunnerToServer` frame so the
    ///      supervisor can broadcast WS events keyed on it (the
    ///      SPA's existing per-thread token-delta plumbing).
    pub conversation_id: String,
    /// The execution unit. Same across all threads from the same
    /// `(channel, principals)` group.
    pub group_id: String,
    /// Latest user message that triggered this turn.
    pub user_text: String,
    /// Principal id of whoever sent `user_text`. Driver for the
    /// runner's per-tool capability checks (which still happen on
    /// the server side via `ToolCallRequest` callbacks; this is
    /// just informational so the runner can log + the model can
    /// see "from: alice").
    pub sender_principal_id: String,
    /// Trust-class tag of the sender (`"Controller"` /
    /// `"KnownTrusted"` / etc.) — informational on the runner side.
    pub sender_trust_class: String,
    /// Composed system prompt (personality + restraint base). The
    /// runner does NOT re-derive this; the supervisor owns
    /// `assemble_system_prompt` so personality edits propagate
    /// without a runner restart.
    pub system_prompt: String,
    /// Replayed conversation history, oldest-first. The supervisor
    /// hydrated this from the event log; the runner just appends
    /// the new user_text and feeds the lot to the model.
    pub history: Vec<ChatMessage>,
    /// Tools the runner is allowed to advertise to the model on
    /// this turn. The supervisor pre-filters by trust class +
    /// `config_tool_access` so the runner doesn't need to know the
    /// policy.
    pub tool_catalog: Vec<ToolDeclaration>,
    /// vLLM endpoint base URL the runner should hit. The runner
    /// doesn't share the supervisor's network namespace; the
    /// supervisor resolves this per turn (so a backend re-spawn
    /// takes effect immediately) and passes it down.
    pub inference_url: String,
    /// Inference model id (e.g. `QuantTrio/Qwen3.5-27B-AWQ`).
    pub model: String,
    /// Optional sampling overrides. None = let the inference server
    /// use its defaults.
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Forwarded into `chat_template_kwargs.enable_thinking` to
    /// suppress / unlock Qwen's `<think>` blocks.
    pub reasoning_enabled: bool,
    /// §7.4 spotlighting delimiter for wrapping untrusted-content
    /// user messages. None = spotlighting off for this turn.
    pub spotlight: Option<String>,
    /// 2026-05-15 — image attachments on the just-arrived user turn,
    /// encoded as `data:<mime>;base64,<bytes>` URLs by the supervisor.
    /// When non-empty, the runner builds the final user message via
    /// `ChatMessage::user_with_images(user_text, user_image_urls)`
    /// instead of a plain-text `ChatMessage::user(user_text)` so the
    /// inference backend receives an OpenAI vision content array.
    /// Backward-compatible default: an older supervisor that doesn't
    /// send the field deserialises as `Vec::new()` and the runner
    /// uses the prior text-only path.
    #[serde(default)]
    pub user_image_urls: Vec<String>,
    /// 2026-05-16 — operator-configured per-turn cap on tool-call
    /// rounds (`config_general.max_tool_rounds`, default 16 server-
    /// side). The runner clamps to its own belt-and-suspenders
    /// `RUNNER_MAX_TOOL_ROUNDS` (24) so a misconfigured server can't
    /// pin the runner in an unbounded loop. Backward-compatible
    /// default `default_max_tool_rounds` keeps older supervisors
    /// (and any future test fixture that omits the field) working.
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u32,
}

/// Conservative default mirroring `crate::server::state::ServerConfig`'s
/// default. Used when an older supervisor deserialises into a newer
/// runner that expects the field. 16 is the same hard floor the
/// in-process executor's tests use.
fn default_max_tool_rounds() -> u32 {
    16
}

/// Server → runner reply to a `ToolCallRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub turn_id: String,
    pub call_id: String,
    pub outcome: ToolOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok { value: serde_json::Value },
    Err { message: String },
}

// ---------------------------------------------------------------------------
// Runner → Server
// ---------------------------------------------------------------------------

/// Frames the runner pushes to the supervisor. Same tagged-enum
/// encoding rules as `ServerToRunner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunnerToServer {
    /// Per-token delta from the streaming inference response. The
    /// supervisor fans these out as `ChatTokenDelta` WS events
    /// keyed on `conversation_id`.
    TokenDelta {
        turn_id: String,
        conversation_id: String,
        text: String,
    },

    /// Conversation phase change (`thinking` / `awaiting_tool` /
    /// `idle`). Supervisor publishes a matching
    /// `ConversationPhaseChanged` event.
    Phase {
        turn_id: String,
        conversation_id: String,
        phase: String,
    },

    /// Runner wants to call a tool. Supervisor dispatches via the
    /// existing `ChainedToolDispatch` (capability-gated), then
    /// sends back `ServerToRunner::ToolCallResult`. Runner blocks
    /// its model loop until the result lands.
    ToolCallRequest {
        turn_id: String,
        conversation_id: String,
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },

    /// Runner wants to append an event to the canonical log. The
    /// supervisor signs (HMAC) + commits + broadcasts. SQLite stays
    /// single-writer.
    EventLogAppend {
        turn_id: String,
        conversation_id: String,
        /// Event kind tag (e.g. `"tool_use"`, `"tool_result"`,
        /// `"model_turn"`). Wire name `event_kind` so it doesn't
        /// collide with the tagged-enum's outer `kind`
        /// discriminator.
        #[serde(rename = "event_kind")]
        kind: String,
        payload: serde_json::Value,
        actor: Option<String>,
    },

    /// Top-level turn finished successfully. Supervisor commits any
    /// remaining events, decrements `in_flight_turns`, and broadcasts
    /// the final `chat_message_outbound`. After this frame, the
    /// runner returns to idle and is eligible for the idle reaper
    /// once `IDLE_TTL` elapses.
    TurnComplete {
        turn_id: String,
        conversation_id: String,
        assistant_text: String,
        finish_reason: Option<String>,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    },

    /// Turn ended with an error. Supervisor surfaces this to the
    /// chat handler awaiting the turn (the SPA gets a banner).
    /// Runner stays alive for the next turn.
    Error {
        turn_id: String,
        conversation_id: String,
        message: String,
        /// True when this error is the runner honouring a
        /// `CancelTurn`. Lets the supervisor distinguish "operator
        /// stopped this turn" from "runner crashed" in metrics +
        /// banner copy.
        cancelled: bool,
    },

    /// Reply to a `ServerToRunner::Heartbeat`. The supervisor uses
    /// the round-trip time as a cheap liveness signal.
    HeartbeatAck { nonce: u64 },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol version mismatch: server={server} runner={runner}")]
    VersionMismatch { server: u32, runner: u32 },
    #[error("decode error: {0}")]
    Decode(String),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_inference_api::Role;

    #[test]
    fn protocol_version_is_stable() {
        // Bumping this number is a wire change. Tests across the
        // workspace pin it as a tripwire — if you bump it,
        // double-check both sides handle the bump AND rebuild
        // the runner Docker image from the matching source tree.
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn turn_request_roundtrips_through_serde_json() {
        let req = TurnRequest {
            turn_id: "t-1".into(),
            conversation_id: "conv-abc".into(),
            group_id: "grp-xyz".into(),
            user_text: "hi".into(),
            sender_principal_id: "controller".into(),
            sender_trust_class: "Controller".into(),
            system_prompt: "you are execlaw".into(),
            history: vec![ChatMessage {
                role: Role::User,
                content: Some(execlaw_inference_api::MessageContent::Text("prior".into())),
                tool_call_id: None,
                name: None,
                tool_calls: vec![],
            }],
            tool_catalog: vec![],
            inference_url: "http://127.0.0.1:8101/v1".into(),
            model: "qwen3.5".into(),
            temperature: Some(0.2),
            max_tokens: None,
            reasoning_enabled: false,
            spotlight: None,
            user_image_urls: Vec::new(),
            max_tool_rounds: 8,
        };
        let s1 = serde_json::to_string(&req).unwrap();
        let back: TurnRequest = serde_json::from_str(&s1).unwrap();
        // ChatMessage doesn't impl PartialEq; compare via re-serialised
        // form which is canonical and field-by-field.
        let s2 = serde_json::to_string(&back).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(back.max_tool_rounds, 8);
    }

    /// 2026-05-16 — fix #5 backward compat: a supervisor that doesn't
    /// send `max_tool_rounds` (older version, test fixture, etc.) must
    /// deserialise into the default value, not fail or land at 0. The
    /// default mirrors the in-process executor's value (16) and lives
    /// well below the runner's `RUNNER_MAX_TOOL_ROUNDS` ceiling (24).
    #[test]
    fn turn_request_max_tool_rounds_defaults_when_field_omitted() {
        let json_no_field = serde_json::json!({
            "turn_id": "t",
            "conversation_id": "c",
            "group_id": "g",
            "user_text": "hi",
            "sender_principal_id": "controller",
            "sender_trust_class": "Controller",
            "system_prompt": "sp",
            "history": [],
            "tool_catalog": [],
            "inference_url": "http://127.0.0.1:8101/v1",
            "model": "qwen3.5",
            "temperature": null,
            "max_tokens": null,
            "reasoning_enabled": false,
            "spotlight": null,
        });
        let parsed: TurnRequest = serde_json::from_value(json_no_field).unwrap();
        assert_eq!(
            parsed.max_tool_rounds, 16,
            "missing max_tool_rounds must default to 16 for backward compat"
        );
    }

    #[test]
    fn server_to_runner_tags_variants() {
        let v = ServerToRunner::CancelTurn {
            turn_id: "t-1".into(),
        };
        let s = serde_json::to_string(&v).unwrap();
        // The tagged-enum format must wire-encode `kind`.
        assert!(s.contains("\"kind\""), "expected kind discriminator: {s}");
        assert!(s.contains("\"cancel_turn\""), "snake_case rename: {s}");
    }

    #[test]
    fn runner_to_server_token_delta_roundtrips() {
        let v = RunnerToServer::TokenDelta {
            turn_id: "t-1".into(),
            conversation_id: "conv-x".into(),
            text: "hello".into(),
        };
        let s1 = serde_json::to_string(&v).unwrap();
        let back: RunnerToServer = serde_json::from_str(&s1).unwrap();
        let s2 = serde_json::to_string(&back).unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn tool_outcome_ok_and_err_roundtrip() {
        let ok = ToolOutcome::Ok {
            value: serde_json::json!({"ok": true}),
        };
        let err = ToolOutcome::Err {
            message: "boom".into(),
        };
        for v in [ok, err] {
            let s1 = serde_json::to_string(&v).unwrap();
            let back: ToolOutcome = serde_json::from_str(&s1).unwrap();
            let s2 = serde_json::to_string(&back).unwrap();
            assert_eq!(s1, s2);
        }
    }

    #[test]
    fn shutdown_reason_serialises_snake_case() {
        let cases = [
            (ShutdownReason::IdleReap, "idle_reap"),
            (ShutdownReason::OperatorRestart, "operator_restart"),
            (ShutdownReason::OperatorWipe, "operator_wipe"),
            (ShutdownReason::ServerShutdown, "server_shutdown"),
            (ShutdownReason::GroupDeleted, "group_deleted"),
        ];
        for (variant, wire) in cases {
            let s = serde_json::to_string(&variant).unwrap();
            assert!(
                s.contains(wire),
                "{:?} should serialise as {}: {}",
                variant,
                wire,
                s
            );
            let back: ShutdownReason = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn registration_ack_roundtrips() {
        let ack = RegistrationAck {
            protocol_version: PROTOCOL_VERSION,
            group_id: "grp-1".into(),
            server_time_ms: 1_700_000_000_000,
        };
        let s = serde_json::to_string(&ack).unwrap();
        let back: RegistrationAck = serde_json::from_str(&s).unwrap();
        assert_eq!(ack, back);
    }

    #[test]
    fn unknown_variant_decodes_as_error_not_panic() {
        // Forward-compat: a frame with an unknown `kind` should
        // surface as a serde error, not a panic.
        let s = r#"{"kind": "future_kind", "turn_id": "t-1"}"#;
        let r: Result<RunnerToServer, _> = serde_json::from_str(s);
        assert!(r.is_err());
    }
}
