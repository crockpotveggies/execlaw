//! `Capabilities` — packed view of [`RegisteredReplyHandler`]'s
//! capability matrix, used by the degradation logic.

use execlaw_core::event_registry::RegisteredReplyHandler;

#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Plugin id (or `"core"` for built-ins) — the dispatch key for
    /// the handler runtime.
    pub plugin_id: String,
    /// Handler name (matches `OriginRef::PluginChannel.plugin_id`).
    pub name: String,

    pub supports_streaming: bool,
    pub supports_attachments: bool,
    pub supports_inline_chart: bool,
    pub supports_table: bool,
    pub supports_card: bool,
    pub supports_markdown: bool,

    pub max_attachment_size_bytes: Option<u64>,
    pub max_attachments_per_message: Option<u32>,
    pub max_text_length: Option<u32>,
    pub allowed_mime_prefixes: Option<Vec<String>>,
}

impl Capabilities {
    pub fn from_registered(h: RegisteredReplyHandler) -> Self {
        Self {
            plugin_id: h.plugin_id,
            name: h.name,
            supports_streaming: h.supports_streaming,
            supports_attachments: h.supports_attachments,
            supports_inline_chart: h.supports_inline_chart,
            supports_table: h.supports_table,
            supports_card: h.supports_card,
            supports_markdown: h.supports_markdown,
            max_attachment_size_bytes: h.max_attachment_size_bytes,
            max_attachments_per_message: h.max_attachments_per_message,
            max_text_length: h.max_text_length,
            allowed_mime_prefixes: h.allowed_mime_prefixes,
        }
    }

    /// Truthy iff this handler is one of the built-in core handlers
    /// (web_socket_session, chat_append, alert, drop). Plugin
    /// handlers go through the `plugin_host.call_tool` path instead.
    pub fn is_core(&self) -> bool {
        self.plugin_id == "core"
    }

    /// Truthy iff the mime is on the handler's allowlist. `None` =
    /// any mime allowed. Empty list = no attachments at all.
    pub fn mime_allowed(&self, mime: &str) -> bool {
        match &self.allowed_mime_prefixes {
            None => true,
            Some(prefixes) => prefixes.iter().any(|p| mime.starts_with(p)),
        }
    }
}
