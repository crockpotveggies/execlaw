//! [`RhaiBackedTransport`] — a generic [`TransportApi`] impl that
//! delegates every method to a Rhai plugin's `tool_call` dispatcher.
//!
//! Why this exists: pre-Phase-B, each channel plugin (Signal today,
//! future WhatsApp / WhatsApp / SMS / Discord) had its own host-
//! side `<channel>_transport.rs` file implementing `TransportApi`.
//! The auto-bridge sites (`bridge_text_reply_to_originating_transport`,
//! attachment fan-out, research-PDF bridge) reached the right impl
//! through `HostTransportRegistry`.
//!
//! Phase B moves channel-specific code into the plugin's ZIP. The
//! plugin still needs to expose outbound capabilities to the
//! auto-bridge — we satisfy that by making `RhaiBackedTransport`
//! the universal `TransportApi` adapter. Each method dispatches
//! to a conventionally-named tool on the plugin:
//!
//!   * `send`                  → `<channel>.send_message({to, text})`
//!   * `send_with_attachments` → `<channel>.send_with_attachments(
//!                                  {to, text, attachments})`
//!   * `start_typing`          → `<channel>.set_typing({to, active: true})`
//!   * `stop_typing`           → `<channel>.set_typing({to, active: false})`
//!   * `list_groups`           → `<channel>.list_groups({})`
//!   * `fetch_attachment`      → `<channel>.fetch_attachment({attachment_id})`
//!
//! `current_chat_id` is set at construction time (per-turn binding
//! lookup, same as the old `SignalCliTransport`); plugins don't
//! need to implement anything for it.
//!
//! `resolve_recipient` reads `state_transport_bindings` directly —
//! identical behaviour to the retired `SignalCliTransport` (this
//! is host-side state, not plugin-owned).
//!
//! ### What gets re-used vs replaced
//!
//! - `HostTransportRegistry` / `HostTransportFactory` — UNCHANGED.
//!   The factory now returns `RhaiBackedTransport` instead of a
//!   per-channel concrete impl.
//! - Auto-bridge sites — UNCHANGED. They walk the registry the
//!   same way; the transport they get back routes through Rhai
//!   instead of native HTTP.
//! - Plugin manifest — gains optional `[transport]` knobs the
//!   factory reads (icon, send-tool name overrides). Most plugins
//!   accept the conventions and don't need to declare anything.

use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::ids::ConversationId;
use execlaw_core::tool::{
    ApiError, FetchedAttachment, TransportApi, TransportGroupSummary,
};
use execlaw_core::transport_bindings::TransportBindingStore;
use execlaw_plugin_host::PluginHost;
use std::sync::Arc;

/// Generic plugin-tool-backed transport. Cheap to clone (Arc inside).
pub struct RhaiBackedTransport {
    plugin_host: PluginHost,
    plugin_id: String,
    channel: String,
    /// Inbound foreign id for the turn that built this transport.
    /// `signal.reply` reads this on dispatch to pick the recipient.
    /// None for transport-builds outside an inbound-triggered turn.
    current_chat_id: Option<String>,
    db: Database,
}

impl RhaiBackedTransport {
    pub fn new(
        plugin_host: PluginHost,
        plugin_id: impl Into<String>,
        channel: impl Into<String>,
        db: Database,
        current_chat_id: Option<String>,
    ) -> Self {
        Self {
            plugin_host,
            plugin_id: plugin_id.into(),
            channel: channel.into(),
            current_chat_id,
            db,
        }
    }

    /// Tool-name convention: `<channel>.<verb>`. Documented above.
    fn tool_name(&self, verb: &str) -> String {
        format!("{}.{verb}", self.channel)
    }

    /// Dispatch a tool call into the plugin. Wraps the host's
    /// `call_tool` chain — caps + trust-floor checks gated to
    /// Controller for outbound bridging (the host owns trust scope
    /// for these tools by virtue of being the calling actor here).
    async fn dispatch(
        &self,
        verb: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let tool_name = self.tool_name(verb);
        // Auto-bridge invocations from the host run with full
        // capability set + Controller trust class — the host is the
        // actor, not the operator's caller principal.
        let caller_caps: &[&str] = &["*"];
        let caller_trust = Some("Controller");
        self.plugin_host
            .call_tool(&tool_name, args, caller_caps, caller_trust)
            .await
            .map_err(|e| ApiError::Storage(format!("plugin tool {tool_name}: {e}")))
    }
}

#[async_trait]
impl TransportApi for RhaiBackedTransport {
    async fn resolve_recipient(
        &self,
        channel: &str,
        free_form: &str,
    ) -> Result<String, ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot resolve channel '{channel}'",
                self.channel
            )));
        }
        let store = TransportBindingStore::new(&self.db);
        match store.lookup_principal_group(channel, free_form) {
            Ok(Some(_)) => Ok(free_form.to_owned()),
            Ok(None) => Err(ApiError::NotFound(format!(
                "no {channel} binding for '{free_form}'"
            ))),
            Err(e) => Err(ApiError::Storage(format!("transport binding lookup: {e}"))),
        }
    }

    async fn send(
        &self,
        channel: &str,
        recipient: &str,
        text: &str,
    ) -> Result<String, ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot send on channel '{channel}'",
                self.channel
            )));
        }
        if recipient.is_empty() {
            return Err(ApiError::Validation("recipient must not be empty".into()));
        }
        if text.is_empty() {
            return Err(ApiError::Validation(
                "message text must not be empty".into(),
            ));
        }
        let r = self
            .dispatch(
                "send_message",
                serde_json::json!({"to": recipient, "text": text}),
            )
            .await?;
        // Plugin returns either a string id or {timestamp: ...}.
        if let Some(s) = r.as_str() {
            return Ok(s.to_owned());
        }
        if let Some(t) = r.get("timestamp") {
            return Ok(t.to_string());
        }
        Ok(r.to_string())
    }

    async fn current_chat_id(&self, channel: &str) -> Result<Option<String>, ApiError> {
        if channel != self.channel {
            return Ok(None);
        }
        Ok(self.current_chat_id.clone())
    }

    async fn list_groups(
        &self,
        channel: &str,
    ) -> Result<Vec<TransportGroupSummary>, ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot list groups on channel '{channel}'",
                self.channel
            )));
        }
        let r = self.dispatch("list_groups", serde_json::json!({})).await?;
        let arr = r
            .get("groups")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "{}.list_groups returned non-array `groups`",
                    self.channel
                ))
            })?;
        let mut out = Vec::with_capacity(arr.len());
        for g in arr {
            let id = g
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ApiError::Storage("group entry missing `id`".into())
                })?
                .to_owned();
            let name = g
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let member_count = g
                .get("member_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(0);
            out.push(TransportGroupSummary {
                id,
                name,
                member_count,
            });
        }
        Ok(out)
    }

    async fn create_group(
        &self,
        channel: &str,
        title: Option<&str>,
        members: &[String],
    ) -> Result<String, ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot create groups on channel '{channel}'",
                self.channel
            )));
        }
        let r = self
            .dispatch(
                "create_group",
                serde_json::json!({"title": title, "members": members}),
            )
            .await?;
        r.get("group_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "{}.create_group returned no `group_id`",
                    self.channel
                ))
            })
    }

    async fn add_group_members(
        &self,
        channel: &str,
        group_id: &str,
        members: &[String],
    ) -> Result<(), ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot add members on channel '{channel}'",
                self.channel
            )));
        }
        let _ = self
            .dispatch(
                "add_group_members",
                serde_json::json!({"group_id": group_id, "members": members}),
            )
            .await?;
        Ok(())
    }

    async fn leave_group(
        &self,
        channel: &str,
        group_id: &str,
    ) -> Result<(), ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot leave groups on channel '{channel}'",
                self.channel
            )));
        }
        let _ = self
            .dispatch("leave_group", serde_json::json!({"group_id": group_id}))
            .await?;
        Ok(())
    }

    async fn fetch_attachment(
        &self,
        channel: &str,
        attachment_id: &str,
    ) -> Result<FetchedAttachment, ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot fetch attachments on channel '{channel}'",
                self.channel
            )));
        }
        let r = self
            .dispatch(
                "fetch_attachment",
                serde_json::json!({"attachment_id": attachment_id}),
            )
            .await?;
        // Plugin returns {bytes_base64, mime_type, filename?}.
        use base64::Engine as _;
        let bytes_b64 = r
            .get("bytes_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "{}.fetch_attachment returned no `bytes_base64`",
                    self.channel
                ))
            })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(bytes_b64)
            .map_err(|e| ApiError::Storage(format!("decode attachment bytes: {e}")))?;
        let mime_type = r
            .get("mime_type")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        let filename = r
            .get("filename")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        Ok(FetchedAttachment {
            bytes,
            mime_type,
            filename,
        })
    }

    async fn send_with_attachments(
        &self,
        channel: &str,
        recipient: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<String, ApiError> {
        if channel != self.channel {
            return Err(ApiError::Validation(format!(
                "{} transport cannot send on channel '{channel}'",
                self.channel
            )));
        }
        if recipient.is_empty() {
            return Err(ApiError::Validation("recipient must not be empty".into()));
        }
        if attachments.is_empty() {
            return self.send(channel, recipient, text).await;
        }
        let r = self
            .dispatch(
                "send_with_attachments",
                serde_json::json!({
                    "to": recipient,
                    "text": text,
                    "attachments": attachments,
                }),
            )
            .await?;
        if let Some(s) = r.as_str() {
            return Ok(s.to_owned());
        }
        if let Some(t) = r.get("timestamp") {
            return Ok(t.to_string());
        }
        Ok(r.to_string())
    }

    async fn start_typing(
        &self,
        channel: &str,
        recipient: &str,
    ) -> Result<(), ApiError> {
        if channel != self.channel {
            return Ok(());
        }
        let _ = self
            .dispatch(
                "set_typing",
                serde_json::json!({"to": recipient, "active": true}),
            )
            .await?;
        Ok(())
    }

    async fn stop_typing(
        &self,
        channel: &str,
        recipient: &str,
    ) -> Result<(), ApiError> {
        if channel != self.channel {
            return Ok(());
        }
        let _ = self
            .dispatch(
                "set_typing",
                serde_json::json!({"to": recipient, "active": false}),
            )
            .await?;
        Ok(())
    }
}

/// Channel-keyed factory for the host transport registry. Reads
/// the plugin manifest's `[transport].icon` field at construction.
/// Replaces the per-channel concrete factories (e.g.
/// `SignalCliTransportFactory`) — every channel plugin uses this.
pub struct RhaiBackedTransportFactory {
    plugin_host: PluginHost,
    plugin_id: String,
    channel: String,
    icon: String,
}

impl RhaiBackedTransportFactory {
    pub fn new(
        plugin_host: PluginHost,
        plugin_id: impl Into<String>,
        channel: impl Into<String>,
        icon: impl Into<String>,
    ) -> Self {
        Self {
            plugin_host,
            plugin_id: plugin_id.into(),
            channel: channel.into(),
            icon: icon.into(),
        }
    }
}

#[async_trait]
impl crate::transport_registry::HostTransportFactory for RhaiBackedTransportFactory {
    fn channel(&self) -> &str {
        &self.channel
    }
    fn icon(&self) -> &str {
        &self.icon
    }
    fn build(
        &self,
        db: &Database,
        _conversation_id: &ConversationId,
        foreign_id: &str,
        is_group: bool,
    ) -> Option<(Arc<dyn TransportApi>, String)> {
        // Recipient transformation: signal-cli's outbound `id` form
        // for groups requires `group.<base64-of-internal-id>`. For
        // the generic transport, we expose this as a transform the
        // plugin can implement via its own `recipient_transform`
        // tool (hit at this method's call site). For now the host
        // applies a default: groups get `group.` + base64; DMs pass
        // through. Plugins that need a different transformation
        // (future channels with non-base64 group ids) override by
        // implementing `<channel>.transform_recipient` and the
        // factory checks for that tool — but that's a follow-up.
        let wire_recipient = if is_group {
            use base64::Engine as _;
            format!(
                "group.{}",
                base64::engine::general_purpose::STANDARD.encode(foreign_id.as_bytes())
            )
        } else {
            foreign_id.to_owned()
        };
        let transport = RhaiBackedTransport::new(
            self.plugin_host.clone(),
            &self.plugin_id,
            &self.channel,
            db.clone(),
            Some(wire_recipient.clone()),
        );
        Some((Arc::new(transport), wire_recipient))
    }
}
