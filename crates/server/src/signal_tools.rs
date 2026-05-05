//! `Arc<dyn ToolImpl>` builtins for the Signal plugin's two
//! host-implemented tools (Phase 3).
//!
//! Every other plugin tool lands as a rhai-tier dispatch through the
//! plugin host. Signal's `send_message` and `reply` need the
//! `TransportApi` capability the rhai tier can't reach, so we lift
//! their implementation up here as builtins. The plugin manifest
//! still owns the user-facing description + trust_floor + group-op
//! coverage; this module only owns the dispatch path for the two
//! tools that need host-side capability injection.
//!
//! Both tools declare `Capability::Transport` so the dispatcher's
//! `build_ctx_for` populates `ctx.transport`. When the supervisor
//! hasn't published a host port yet (sidecar still starting,
//! crashed-and-respawning, or never registered), `ctx.transport`
//! itself is wired but the underlying `send` call returns
//! `ApiError::Storage("signal-cli sidecar … not running yet")` —
//! propagated as a tool-error code the chat surface humanises into
//! the existing "Sending Signal message — failed: …" line.

use async_trait::async_trait;
use execlaw_core::tool::{
    Capability, ToolCtx, ToolDescriptor, ToolImpl, ToolLatency, ToolOutcome, ToolSource,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::signal_transport::SIGNAL_CHANNEL;

// ---- send_message ----------------------------------------------

#[derive(Debug, Deserialize)]
struct SendMessageArgs {
    /// Free-form recipient: a display name, phone number, or
    /// canonical `signal:user:<uuid>` foreign id. The transport
    /// resolves this into a binding before dispatching.
    to: String,
    /// Message text. Empty strings are rejected by the transport
    /// rather than silently sending whitespace.
    text: String,
    /// Phase 7 — optional list of `state_attachments` ids to attach
    /// to the outbound message. Each must belong to the calling
    /// conversation (the transport enforces the scope check); a
    /// miss surfaces as a tool-error rather than a silent partial
    /// send.
    #[serde(default)]
    attachment_ids: Vec<String>,
}

pub struct SignalSendMessageTool {
    descriptor: ToolDescriptor,
}

impl Default for SignalSendMessageTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalSendMessageTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "signal.send_message".into(),
                description: "Send a Signal message to a person or group. \
                     This tool IS the user-visible message — do NOT also produce a text \
                     reply summarising what you sent. After calling this tool, return \
                     an empty text response to end your turn. \
                     Args: { to: string (name, phone, or `signal:user:<uuid>` JID), text: string, \
                     attachment_ids?: string[] (optional state_attachments ids — each must \
                     belong to this conversation; max 8 per send) }."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "to": {
                            "type": "string",
                            "description": "Recipient — display name, phone number, or signal foreign id."
                        },
                        "text": {
                            "type": "string",
                            "description": "Message body. May be empty when sending only attachments."
                        },
                        "attachment_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "maxItems": 8,
                            "description": "Optional state_attachments ids to attach. Each must belong to this conversation. Max 8 per send.",
                        }
                    },
                    "required": ["to", "text"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::Transport],
                // Controller-only: the manifest's `trust_floor =
                // "Controller"` is enforced through the access gate
                // by setting `default_allowed_classes = ["Controller"]`.
                // A Signal contact (KnownTrusted / KnownLimited)
                // mustn't be able to ask the agent to message OTHER
                // people via the controller's outbound transport —
                // selfhosted-claw learned this the hard way.
                default_allowed_classes: vec!["Controller".into()],
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SignalSendMessageTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: SendMessageArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let transport = match ctx.transport.as_ref() {
            Some(t) => t,
            None => {
                return ToolOutcome::denied("transport capability not granted to this tool");
            }
        };
        let recipient = match transport.resolve_recipient(SIGNAL_CHANNEL, &args.to).await {
            Ok(r) => r,
            Err(e) => return e.into_outcome(),
        };
        // Phase 7 — branch on attachment_ids. Empty list (the
        // common case) flows through `send`; non-empty switches
        // to `send_with_attachments` which validates each id's
        // conversation scope and base64-encodes the bytes for
        // signal-cli's `base64_attachments` field.
        let send_result = if args.attachment_ids.is_empty() {
            transport.send(SIGNAL_CHANNEL, &recipient, &args.text).await
        } else {
            transport
                .send_with_attachments(SIGNAL_CHANNEL, &recipient, &args.text, &args.attachment_ids)
                .await
        };
        match send_result {
            Ok(message_id) => ToolOutcome::Ok(json!({
                "message_id": message_id,
                "recipient": recipient,
                "attachment_count": args.attachment_ids.len(),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---- reply -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReplyArgs {
    text: String,
}

pub struct SignalReplyTool {
    descriptor: ToolDescriptor,
}

impl Default for SignalReplyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalReplyTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "signal.reply".into(),
                description: "Reply in the current Signal conversation. \
                     This tool IS the user-visible reply — do NOT also produce a text \
                     response summarising what you replied. After calling this tool, \
                     return an empty text response to end your turn. \
                     Only available when the current turn was triggered by an inbound \
                     Signal message; the sidecar derives the recipient from the \
                     conversation's principal_group. Args: { text: string }."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Reply body. Must not be empty."
                        }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::Transport],
                // Reply is the inverse of send_message: by definition
                // the inbound principal IS the recipient, so a
                // KnownLimited contact replying on their own thread is
                // safe. Allowlist all non-blocked classes — the
                // access-gate enforcement collapses to "is this turn
                // even allowed to run." Mirrors the manifest's
                // deliberate omission of trust_floor for reply.
                default_allowed_classes: vec![
                    "Controller".into(),
                    "Delegated".into(),
                    "KnownTrusted".into(),
                    "KnownLimited".into(),
                    "UnknownPending".into(),
                ],
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SignalReplyTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ReplyArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let transport = match ctx.transport.as_ref() {
            Some(t) => t,
            None => {
                return ToolOutcome::denied("transport capability not granted to this tool");
            }
        };
        // current_chat_id is the inbound foreign id resolved by
        // `tool_dispatch::build_ctx_for` at dispatcher-build time
        // (Phase 4): conversation_id → principal_group_id →
        // bindings_for_group("signal") → foreign_id. `None` means
        // either the turn wasn't triggered on a Signal-bound
        // conversation, or the binding lookup chain failed mid-way
        // (no row, NULL principal_group_id, no signal binding for
        // the group). Surfaces as a tool-error (not denial) because
        // it's a precondition violation, not a permission issue.
        let recipient = match transport.current_chat_id(SIGNAL_CHANNEL).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return ToolOutcome::err(
                    "no_inbound_context",
                    "signal.reply is only available on turns triggered by an inbound \
                     Signal message; this turn was not. Use signal.send_message with \
                     an explicit recipient instead.",
                );
            }
            Err(e) => return e.into_outcome(),
        };
        match transport.send(SIGNAL_CHANNEL, &recipient, &args.text).await {
            Ok(message_id) => ToolOutcome::Ok(json!({
                "message_id": message_id,
                "recipient": recipient,
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---- list_groups (Phase 5) -------------------------------------

pub struct SignalListGroupsTool {
    descriptor: ToolDescriptor,
}

impl Default for SignalListGroupsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalListGroupsTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "signal.list_groups".into(),
                description: "List Signal groups the controller is a member of. \
                     Returns: { groups: [{ id, name, member_count }] }."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::Transport],
                default_allowed_classes: vec!["Controller".into()],
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SignalListGroupsTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let transport = match ctx.transport.as_ref() {
            Some(t) => t,
            None => return ToolOutcome::denied("transport capability not granted to this tool"),
        };
        match transport.list_groups(SIGNAL_CHANNEL).await {
            Ok(groups) => ToolOutcome::Ok(json!({
                "groups": groups
                    .into_iter()
                    .map(|g| json!({
                        "id": g.id,
                        "name": g.name,
                        "member_count": g.member_count,
                    }))
                    .collect::<Vec<_>>(),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---- create_group ---------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateGroupArgs {
    /// Optional group title. signal-cli-rest-api accepts an empty
    /// string for nameless groups; the transport handles the
    /// translation.
    #[serde(default)]
    title: Option<String>,
    /// Member identifiers — display names, phone numbers, or
    /// canonical foreign ids. Each is resolved via
    /// `transport.resolve_recipient` before the bridge call.
    members: Vec<String>,
    /// Optional first message — sent immediately after the group
    /// is created so members see context for why they were added.
    /// Empty / missing leaves the group silent until the agent
    /// (or operator) sends manually.
    #[serde(default)]
    message: Option<String>,
}

pub struct SignalCreateGroupTool {
    descriptor: ToolDescriptor,
}

impl Default for SignalCreateGroupTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalCreateGroupTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "signal.create_group".into(),
                description: "Create a new Signal group with the specified members. \
                     The tool result includes an `ack_text` field — your final reply to \
                     the user MUST be exactly that ack_text with no additions, no emoji, \
                     and no rephrasing. \
                     Args: { title?: string, members: string[] (names or phone numbers), \
                     message?: string (initial post) }."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "members": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                        },
                        "message": { "type": "string" }
                    },
                    "required": ["members"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::Transport],
                default_allowed_classes: vec!["Controller".into()],
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SignalCreateGroupTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: CreateGroupArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let transport = match ctx.transport.as_ref() {
            Some(t) => t,
            None => return ToolOutcome::denied("transport capability not granted to this tool"),
        };
        // Resolve each free-form member to a canonical foreign id
        // before the bridge call. A miss here means the operator
        // typed a name we can't bind to a Signal contact — surface
        // it as a tool error rather than dispatching with bad
        // recipients.
        let mut resolved = Vec::with_capacity(args.members.len());
        for raw in &args.members {
            match transport.resolve_recipient(SIGNAL_CHANNEL, raw).await {
                Ok(id) => resolved.push(id),
                Err(e) => return e.into_outcome(),
            }
        }
        let group_id = match transport
            .create_group(SIGNAL_CHANNEL, args.title.as_deref(), &resolved)
            .await
        {
            Ok(id) => id,
            Err(e) => return e.into_outcome(),
        };
        // Optional initial post. Failure to send doesn't unwind the
        // group creation — the group exists either way; surfacing
        // the send error in the tool result lets the agent decide
        // whether to retry.
        let initial_send_id =
            if let Some(text) = args.message.as_deref().filter(|s| !s.trim().is_empty()) {
                match transport.send(SIGNAL_CHANNEL, &group_id, text).await {
                    Ok(id) => Some(id),
                    Err(e) => return e.into_outcome(),
                }
            } else {
                None
            };
        let title_for_ack = args
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("\"{s}\""))
            .unwrap_or_else(|| "(unnamed)".to_owned());
        let ack_text = format!(
            "Created Signal group {} with {} member{}.",
            title_for_ack,
            resolved.len(),
            if resolved.len() == 1 { "" } else { "s" }
        );
        ToolOutcome::Ok(json!({
            "group_id": group_id,
            "members": resolved,
            "initial_send_id": initial_send_id,
            "ack_text": ack_text,
            "agent_instruction": "reply to the user with EXACTLY the value of ack_text — no additions, no emoji, no rephrasing.",
        }))
    }
}

// ---- add_group_members ----------------------------------------

#[derive(Debug, Deserialize)]
struct AddGroupMembersArgs {
    /// Group lookup is by exact name match — keeps the agent's
    /// mental model simple ("the group called Movie Club") even
    /// when signal-cli's canonical identifier is an opaque
    /// base64 blob.
    #[serde(rename = "groupName")]
    group_name: String,
    /// Member identifiers to add — same shape as create_group's
    /// `members`.
    members: Vec<String>,
}

pub struct SignalAddGroupMembersTool {
    descriptor: ToolDescriptor,
}

impl Default for SignalAddGroupMembersTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalAddGroupMembersTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "signal.add_group_members".into(),
                description:
                    "Add members to an existing Signal group, looking up the group by exact name match. \
                     The tool result includes an `ack_text` field — your final reply to \
                     the user MUST be exactly that ack_text with no additions, no emoji, \
                     and no rephrasing. Args: { groupName: string, members: string[] (names \
                     or phone numbers) }."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "groupName": { "type": "string" },
                        "members": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                        }
                    },
                    "required": ["groupName", "members"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::Transport],
                default_allowed_classes: vec!["Controller".into()],
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SignalAddGroupMembersTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: AddGroupMembersArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let transport = match ctx.transport.as_ref() {
            Some(t) => t,
            None => return ToolOutcome::denied("transport capability not granted to this tool"),
        };
        let group_id = match resolve_group_id(transport.as_ref(), &args.group_name).await {
            Ok(id) => id,
            Err(e) => return e.into_outcome(),
        };
        let mut resolved = Vec::with_capacity(args.members.len());
        for raw in &args.members {
            match transport.resolve_recipient(SIGNAL_CHANNEL, raw).await {
                Ok(id) => resolved.push(id),
                Err(e) => return e.into_outcome(),
            }
        }
        if let Err(e) = transport
            .add_group_members(SIGNAL_CHANNEL, &group_id, &resolved)
            .await
        {
            return e.into_outcome();
        }
        let ack_text = format!(
            "Added {} member{} to \"{}\".",
            resolved.len(),
            if resolved.len() == 1 { "" } else { "s" },
            args.group_name,
        );
        ToolOutcome::Ok(json!({
            "group_id": group_id,
            "added": resolved,
            "ack_text": ack_text,
            "agent_instruction": "reply to the user with EXACTLY the value of ack_text — no additions, no emoji, no rephrasing.",
        }))
    }
}

// ---- leave_group ----------------------------------------------

#[derive(Debug, Deserialize)]
struct LeaveGroupArgs {
    #[serde(rename = "groupName")]
    group_name: String,
}

pub struct SignalLeaveGroupTool {
    descriptor: ToolDescriptor,
}

impl Default for SignalLeaveGroupTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalLeaveGroupTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "signal.leave_group".into(),
                description:
                    "Leave an existing Signal group, looking up the group by exact name match. \
                     The tool result includes an `ack_text` field — your final reply to \
                     the user MUST be exactly that ack_text with no additions, no emoji, \
                     and no rephrasing. Args: { groupName: string }."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "groupName": { "type": "string" }
                    },
                    "required": ["groupName"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::Transport],
                default_allowed_classes: vec!["Controller".into()],
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SignalLeaveGroupTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: LeaveGroupArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let transport = match ctx.transport.as_ref() {
            Some(t) => t,
            None => return ToolOutcome::denied("transport capability not granted to this tool"),
        };
        let group_id = match resolve_group_id(transport.as_ref(), &args.group_name).await {
            Ok(id) => id,
            Err(e) => return e.into_outcome(),
        };
        if let Err(e) = transport.leave_group(SIGNAL_CHANNEL, &group_id).await {
            return e.into_outcome();
        }
        let ack_text = format!("Left Signal group \"{}\".", args.group_name);
        ToolOutcome::Ok(json!({
            "group_id": group_id,
            "ack_text": ack_text,
            "agent_instruction": "reply to the user with EXACTLY the value of ack_text — no additions, no emoji, no rephrasing.",
        }))
    }
}

/// Resolve a free-form group name to the bridge's canonical group
/// id by listing all groups and exact-name matching. Empty / missing
/// names return `Validation`; multiple matches return `Validation`
/// (the operator must rename or be more specific); zero matches
/// return `NotFound`. The exact-name semantic is deliberately strict
/// — fuzzy matching here would let "Family" silently target "Family
/// 2" and the agent would notice only after a destructive op.
async fn resolve_group_id(
    transport: &dyn execlaw_core::tool::TransportApi,
    group_name: &str,
) -> Result<String, execlaw_core::tool::ApiError> {
    if group_name.trim().is_empty() {
        return Err(execlaw_core::tool::ApiError::Validation(
            "groupName must not be empty".into(),
        ));
    }
    let groups = transport.list_groups(SIGNAL_CHANNEL).await?;
    let matches: Vec<_> = groups
        .into_iter()
        .filter(|g| g.name.as_deref() == Some(group_name))
        .collect();
    match matches.len() {
        0 => Err(execlaw_core::tool::ApiError::NotFound(format!(
            "no Signal group named \"{group_name}\""
        ))),
        1 => Ok(matches.into_iter().next().unwrap().id),
        n => Err(execlaw_core::tool::ApiError::Validation(format!(
            "{n} Signal groups share the name \"{group_name}\"; \
             rename one or pass an explicit group_id"
        ))),
    }
}

/// All signal builtins (Phase 3 + Phase 5), ready for
/// `register_builtins`. `crates/cli/src/main.rs` calls this after
/// `register_core_builtins` so the access-gate seed runs cleanly.
pub fn signal_builtin_tools() -> Vec<Arc<dyn ToolImpl>> {
    vec![
        Arc::new(SignalSendMessageTool::new()) as Arc<dyn ToolImpl>,
        Arc::new(SignalReplyTool::new()),
        Arc::new(SignalListGroupsTool::new()),
        Arc::new(SignalCreateGroupTool::new()),
        Arc::new(SignalAddGroupMembersTool::new()),
        Arc::new(SignalLeaveGroupTool::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal_transport::{SignalCliTransport, StaticEndpointResolver};
    use execlaw_core::Database;
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::ConversationId;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::tool::{Clock, SystemClock};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn ctx_with_transport(
        db: Database,
        transport: Arc<SignalCliTransport>,
        caller_trust: &str,
    ) -> ToolCtx {
        let _ = db; // tools don't read db directly; transport does
        let mut ctx = ToolCtx::empty(
            ConversationId::from_string("conv-test"),
            caller_trust,
            Arc::new(SystemClock) as Arc<dyn Clock>,
        );
        ctx.transport = Some(transport);
        ctx
    }

    #[tokio::test]
    async fn send_message_returns_storage_error_when_sidecar_unreachable() {
        // No mock listener: the StaticEndpointResolver points at a
        // black-hole port, so the connect timeout fires.
        let resolver = Arc::new(StaticEndpointResolver("http://127.0.0.1:1".into()));
        let db = fresh_db();
        let transport = Arc::new(SignalCliTransport::new(
            resolver,
            db.clone(),
            Some("+15551234567".into()),
            None,
        ));
        // Pre-seed a binding so resolve_recipient succeeds — we want
        // to exercise the network-fail path, not the resolution-fail
        // path.
        execlaw_core::transport_bindings::TransportBindingStore::new(&db)
            .insert_binding(SIGNAL_CHANNEL, "+15559998888", "pg-1", false, 0)
            .unwrap();
        let tool = SignalSendMessageTool::new();
        let ctx = ctx_with_transport(db, transport, "Controller");
        let out = tool
            .invoke(ctx, json!({ "to": "+15559998888", "text": "hi" }))
            .await;
        match out {
            ToolOutcome::Err { code, message } => {
                assert_eq!(code, "storage_error");
                assert!(
                    message.contains("signal-cli") || message.contains("RPC"),
                    "got: {message}"
                );
            }
            other => panic!("expected storage_error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_message_denies_when_transport_capability_missing() {
        // Builds a ToolCtx WITHOUT setting ctx.transport — simulates
        // the dispatcher choosing not to grant the capability. The
        // tool must surface Denied, not unwrap and panic.
        let mut ctx = ToolCtx::empty(
            ConversationId::from_string("conv-test"),
            "Controller",
            Arc::new(SystemClock) as Arc<dyn Clock>,
        );
        ctx.transport = None;
        let tool = SignalSendMessageTool::new();
        let out = tool
            .invoke(ctx, json!({ "to": "+15559998888", "text": "hi" }))
            .await;
        assert!(matches!(out, ToolOutcome::Denied { .. }), "got {out:?}");
    }

    #[tokio::test]
    async fn send_message_rejects_invalid_args() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let transport = Arc::new(SignalCliTransport::new(
            resolver,
            db.clone(),
            Some("+15551234567".into()),
            None,
        ));
        let tool = SignalSendMessageTool::new();
        let ctx = ctx_with_transport(db, transport, "Controller");
        // Missing required field 'text'.
        let out = tool.invoke(ctx, json!({ "to": "+15559998888" })).await;
        match out {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected invalid_argument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reply_errors_when_no_inbound_context() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        // current_chat_id = None — turn wasn't Signal-triggered.
        let transport = Arc::new(SignalCliTransport::new(
            resolver,
            db.clone(),
            Some("+15551234567".into()),
            None,
        ));
        let tool = SignalReplyTool::new();
        let ctx = ctx_with_transport(db, transport, "KnownLimited");
        let out = tool.invoke(ctx, json!({ "text": "ok" })).await;
        match out {
            ToolOutcome::Err { code, message } => {
                assert_eq!(code, "no_inbound_context");
                assert!(message.contains("signal.send_message"), "got: {message}");
            }
            other => panic!("expected no_inbound_context, got {other:?}"),
        }
    }

    #[test]
    fn descriptor_capabilities_include_transport() {
        for tool in signal_builtin_tools() {
            let caps = &tool.descriptor().capabilities;
            assert!(
                caps.iter().any(|c| matches!(c, Capability::Transport)),
                "{} must declare Capability::Transport",
                tool.descriptor().name
            );
        }
    }

    #[test]
    fn send_message_default_allowlist_is_controller_only() {
        let t = SignalSendMessageTool::new();
        assert_eq!(t.descriptor.default_allowed_classes, vec!["Controller"]);
    }

    #[test]
    fn reply_default_allowlist_includes_known_limited() {
        let t = SignalReplyTool::new();
        // Reply must be available to inbound contacts replying on
        // their own thread; KnownLimited is the lowest-trust active
        // class, so its presence pins the policy.
        assert!(
            t.descriptor
                .default_allowed_classes
                .iter()
                .any(|c| c == "KnownLimited"),
            "reply allowlist must include KnownLimited"
        );
    }

    // ---- Phase 5 group ops -------------------------------------

    #[test]
    fn signal_builtin_tools_returns_all_six_tools() {
        let tools = signal_builtin_tools();
        let names: Vec<String> = tools.iter().map(|t| t.descriptor().name.clone()).collect();
        for expected in [
            "signal.send_message",
            "signal.reply",
            "signal.list_groups",
            "signal.create_group",
            "signal.add_group_members",
            "signal.leave_group",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} missing from signal_builtin_tools(); got {names:?}",
            );
        }
    }

    #[test]
    fn group_ops_are_controller_only() {
        for tool in [
            Arc::new(SignalListGroupsTool::new()) as Arc<dyn ToolImpl>,
            Arc::new(SignalCreateGroupTool::new()),
            Arc::new(SignalAddGroupMembersTool::new()),
            Arc::new(SignalLeaveGroupTool::new()),
        ] {
            assert_eq!(
                tool.descriptor().default_allowed_classes,
                vec!["Controller".to_string()],
                "{} must be Controller-only — group ops are all destructive",
                tool.descriptor().name
            );
        }
    }

    #[test]
    fn create_group_descriptor_requires_members_array() {
        let t = SignalCreateGroupTool::new();
        let schema = &t.descriptor.schema;
        // Schema enforces minItems=1 on members — defensive against
        // an agent calling create_group with no recipients.
        let members = schema
            .pointer("/properties/members")
            .expect("members property must exist");
        assert_eq!(members["minItems"], 1);
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "members"));
    }

    #[tokio::test]
    async fn list_groups_denies_when_transport_capability_missing() {
        let mut ctx = ToolCtx::empty(
            ConversationId::from_string("conv-test"),
            "Controller",
            Arc::new(SystemClock) as Arc<dyn Clock>,
        );
        ctx.transport = None;
        let tool = SignalListGroupsTool::new();
        let out = tool.invoke(ctx, json!({})).await;
        assert!(matches!(out, ToolOutcome::Denied { .. }), "got {out:?}");
    }

    #[tokio::test]
    async fn create_group_rejects_empty_members() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let transport = Arc::new(SignalCliTransport::new(
            resolver,
            db.clone(),
            Some("+15551234567".into()),
            None,
        ));
        let tool = SignalCreateGroupTool::new();
        let ctx = ctx_with_transport(db, transport, "Controller");
        // Schema rejects {members: []} via minItems=1 — exercised
        // through serde validation before reaching the bridge.
        let out = tool
            .invoke(ctx, json!({ "members": [], "title": "test" }))
            .await;
        // The transport's own create_group also guards against
        // empty members; either layer can fail. Both are
        // Validation-class errors.
        match out {
            ToolOutcome::Err { code, .. } => {
                assert!(
                    code == "invalid_argument" || code == "storage_error",
                    "got code {code}",
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_group_members_rejects_empty_group_name() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let transport = Arc::new(SignalCliTransport::new(
            resolver,
            db.clone(),
            Some("+15551234567".into()),
            None,
        ));
        let tool = SignalAddGroupMembersTool::new();
        let ctx = ctx_with_transport(db, transport, "Controller");
        let out = tool
            .invoke(
                ctx,
                json!({ "groupName": "   ", "members": ["+15559998888"] }),
            )
            .await;
        // Whitespace-only group name → resolve_group_id returns
        // Validation. The transport never gets a chance to fire.
        match out {
            ToolOutcome::Err { code, message } => {
                assert!(
                    code == "invalid_argument" || message.contains("groupName"),
                    "got code={code}, message={message}",
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leave_group_returns_storage_error_when_sidecar_unreachable() {
        // Tool dispatches list_groups first to resolve groupName →
        // group_id; with no sidecar the list_groups call surfaces a
        // Storage error before we even reach the leave RPC.
        let resolver = Arc::new(StaticEndpointResolver("http://127.0.0.1:1".into()));
        let db = fresh_db();
        let transport = Arc::new(SignalCliTransport::new(
            resolver,
            db.clone(),
            Some("+15551234567".into()),
            None,
        ));
        let tool = SignalLeaveGroupTool::new();
        let ctx = ctx_with_transport(db, transport, "Controller");
        let out = tool
            .invoke(ctx, json!({ "groupName": "Nonexistent" }))
            .await;
        match out {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "storage_error"),
            other => panic!("expected storage_error, got {other:?}"),
        }
    }
}
