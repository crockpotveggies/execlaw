//! Wire types and persisted payloads for the chat surface.
//!
//! No logic lives here; the submodule exists purely to keep the
//! parent module (`crates/server/src/chats.rs`) from drowning in
//! struct definitions. Items that were originally `pub` on the
//! `chats` module stay `pub` (re-exported from `chats.rs`) so
//! external crates and the OpenAPI generator still see them at
//! `crate::chats::SendMessageRequest` etc.
//!
//! Items that were private to `chats.rs` (the `*Payload` event
//! payload structs, the deserialize helper) become `pub(crate)`
//! here with `pub(crate)` field visibility — the parent module is
//! the only consumer and needs to both construct and destructure
//! them. `pub(crate)` keeps them out of the public surface while
//! still letting `chats.rs` move freely across the boundary.

use execlaw_core::conversation::ThreadSummary;
use serde::{Deserialize, Serialize};

// =====================================================================
// Inbound request shapes
// =====================================================================

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SendMessageRequest {
    pub text: String,
    /// Optional override — defaults to the controller's principal id.
    pub sender_principal_id: Option<String>,
    /// 2026-04-28 — when true, run the turn against inference but
    /// skip every persistent write: no event-log rows, no
    /// conversation-table upsert, no outbox, no display-name
    /// generation. The SPA owns the transcript and ships the
    /// running history in `prior_messages` on each turn.
    /// Streaming token deltas + phase events still broadcast over
    /// the WS bus keyed on `conversation_id`, matching the regular
    /// chat UX exactly. Default false.
    #[serde(default)]
    pub incognito: bool,
    /// 2026-04-28 — running transcript for incognito turns. The
    /// server reads this in place of replaying the event log when
    /// `incognito = true`. Ordered oldest-first; excludes the new
    /// user message in `text` (server appends that itself before
    /// calling the model). Each entry's `role` is `"user"` or
    /// `"assistant"`. Ignored when `incognito = false`.
    #[serde(default)]
    pub prior_messages: Vec<IncognitoTurnMessage>,
    /// IANA timezone name (e.g. `America/Los_Angeles`) the SPA
    /// browser detected via `Intl.DateTimeFormat().resolvedOptions().timeZone`.
    /// Stamped into the per-turn context prose so the agent
    /// interprets bare clock times ("create an event at 6pm") in
    /// the operator's local zone instead of UTC. Optional because
    /// non-browser inbounds (Signal, future SMS / email) don't
    /// supply one; those fall back to UTC and the agent should
    /// ask if a clock time is ambiguous.
    #[serde(default)]
    pub timezone: Option<String>,
    /// 2026-05-15 — inline image attachments. Each entry is a
    /// `data:image/...;base64,...` URL the SPA produced from the
    /// operator's file picker. The server decodes, content-
    /// addresses the bytes under `<data_dir>/blobs/`, and inserts a
    /// `state_attachments` row scoped to the conversation. The
    /// resulting attachment ids are stamped onto the `user_msg`
    /// event payload so history replay can re-encode them as
    /// `image_url` content parts when calling a vision-capable
    /// model (Qwen3-VL / Qwen3.6 / LLaVA / Pixtral, etc).
    ///
    /// Per-image data URL is capped at ~20 MiB after base64 decode;
    /// oversize images are rejected with `attachment_too_large`.
    /// The SPA should pre-resize before send (1024-ish px on the
    /// long edge is enough for nearly every vision model and keeps
    /// the request comfortably under the cap).
    #[serde(default)]
    pub attachments: Vec<InlineAttachmentRequest>,
    /// 2026-05-15 — names of skills the operator picked from the
    /// composer's `+` menu to apply to THIS turn only. The server
    /// resolves each name to its current stable/trial body, prepends
    /// the bodies as `<skill name="...">...</skill>` blocks above
    /// the user text, and ships the combined string to the model.
    /// The original (un-prepended) text remains in
    /// `UserMessagePayload.text` so subsequent turns don't keep
    /// re-seeing the skill body in history. The applied names land
    /// on `UserMessagePayload.applied_skill_names` so the SPA can
    /// render a "applied: foo, bar" chip on the message bubble.
    ///
    /// Validation:
    ///   * Unknown / archived skill name → 404 `skill_not_found`.
    ///   * Total resolved body bytes > [`MAX_PREPEND_SKILL_BYTES`]
    ///     → 413 `skill_prepend_too_large` (prevents ballooning the
    ///     request past the model's context window in one shot).
    ///
    /// Empty / absent for every non-web inbound path; transports
    /// don't surface a picker UI today.
    #[serde(default)]
    pub skill_names: Vec<String>,
}

/// Hard cap on the total bytes of resolved skill bodies prepended
/// onto a single turn. Kept generous (32 KiB ≈ 8000 tokens) so a
/// few medium-sized skills fit comfortably; exceeded only when the
/// operator picks several large skills at once. The cap is checked
/// AFTER resolving each name to its body — names alone don't count.
pub(crate) const MAX_PREPEND_SKILL_BYTES: usize = 32 * 1024;

/// One image attachment from the SPA composer. The `data_url` carries
/// the bytes inline as a `data:` URL (`data:<mime>;base64,<bytes>`).
/// The SPA encodes locally so the server doesn't need a separate
/// upload endpoint for the common Phase-1 case.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct InlineAttachmentRequest {
    /// IANA mime type. Server-side acceptlist:
    ///   * Image: `image/png|jpeg|webp|gif` (routed to vision content).
    ///   * Data: `text/csv|tab-separated-values|plain|markdown`,
    ///     `application/json|pdf|xlsx|xls` (routed only to
    ///     `state_attachments` + python-sandbox hydration; agent
    ///     learns via per-turn context block).
    /// Anything else fails with `attachment_mime_unsupported`.
    pub mime: String,
    /// `data:<mime>;base64,<bytes>` URL. The mime in this URL must
    /// match the `mime` field above; mismatches fail with
    /// `attachment_data_url_invalid`.
    pub data_url: String,
    /// 2026-05-18 — original filename from the OS file picker
    /// (e.g. `quarterly-revenue.csv`). Required for non-image
    /// attachments so Phase-3 hydration can drop the file at
    /// `/work/<convo>/uploads/<filename>` and the agent can
    /// reference it by name. Optional for images — they land in
    /// vision content where the filename isn't user-facing.
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct IncognitoTurnMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Return events with `seq > before` in ascending order. Default 0
    /// (return everything).
    #[serde(default)]
    pub before: i64,
    /// Hard cap — default 200.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PatchThreadRequest {
    /// `Some(Some(name))` to set, `Some(None)` to clear, `None` to skip.
    /// Serde maps both `"display_name": null` and a missing field to
    /// `None`; we distinguish via a custom `#[serde(default,
    /// deserialize_with)]` shim so the operator can clear the name.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[schema(value_type = Option<String>)]
    pub display_name: Option<Option<String>>,
    pub is_pinned: Option<bool>,
    /// When `Some(true)` AND `ephemeral_expires_at` is set, marks the
    /// thread incognito with that expiry. When `Some(false)`, clears
    /// the incognito flag (and clears the expiry implicitly).
    pub is_ephemeral: Option<bool>,
    /// Unix-seconds expiry for incognito threads. Only honored when
    /// `is_ephemeral = Some(true)`. Ignored on `Some(false)`.
    pub ephemeral_expires_at: Option<i64>,
}

/// Custom deserializer so `null` and missing are distinct: `None` =
/// missing field (leave alone), `Some(None)` = explicit null (clear),
/// `Some(Some(v))` = set.
pub(crate) fn deserialize_optional_field<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

// =====================================================================
// Outbound response shapes
// =====================================================================

#[derive(Debug, Serialize)]
pub struct SendMessageResponse {
    pub conversation_id: String,
    pub user_msg_seq: i64,
    pub assistant_text: String,
    pub assistant_seq: i64,
}

#[derive(Debug, Serialize)]
pub struct MessagesListResponse {
    pub conversation_id: String,
    pub messages: Vec<MessageView>,
}

#[derive(Debug, Serialize)]
pub struct MessageView {
    pub seq: i64,
    pub kind: String,
    pub text: Option<String>,
    pub actor: Option<String>,
    pub committed_at: i64,
    /// Originating transport for this message (signal / email /
    /// voice / sms). Set on user_msg + model_turn events that
    /// flowed through a transport bridge; absent for the default
    /// web path. The SPA reads this to render a per-message
    /// channel icon in the chat view so the operator can tell at
    /// a glance "this came in via Signal" / "the agent replied
    /// via Signal".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_origin: Option<String>,
    /// 2026-05-15 — image attachments included on a user_msg via
    /// the composer's `+` menu. Empty (and serialised as absent)
    /// for every other message kind. The SPA renders each entry
    /// as an inline `<img src="/api/attachments/{id}">` above the
    /// text bubble.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MessageAttachmentView>,
    /// 2026-05-15 — names of skills the operator picked from the
    /// composer's `+` menu when sending this `user_msg`. The skill
    /// bodies were prepended onto the message text server-side
    /// before the model saw them; the SPA strips those `<skill
    /// name="...">...</skill>` blocks out of `text` for display
    /// and renders this list as a chip under the bubble. Empty for
    /// every non-user_msg kind and for user_msg events sent without
    /// any skill selection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_skill_names: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MessageAttachmentView {
    pub id: String,
    pub mime: String,
    /// 2026-05-18 — operator-facing filename from
    /// `state_attachments.filename`. Required by the SPA's
    /// MessageStream to render non-image attachments as file
    /// chips (icon + filename + download link) instead of as
    /// `<img>` tags. `None` for legacy rows and for
    /// transport-inbound rows that never carried a filename.
    pub filename: Option<String>,
    /// Blob size in bytes — surfaced so the SPA chip can show
    /// "data.csv (5.2 KB)" without a second round-trip. Best-
    /// effort: a stat failure on disk returns 0 (rather than
    /// failing the whole list-messages call).
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct PatchThreadResponse {
    pub conversation_id: String,
    pub display_name: Option<String>,
    pub is_pinned: bool,
    pub is_ephemeral: bool,
    pub ephemeral_expires_at: Option<i64>,
}

/// One thread row in `GET /api/chats`.
#[derive(Debug, Serialize)]
pub struct ThreadSummaryView {
    pub conversation_id: String,
    pub kind: String,
    pub phase: String,
    pub trust_class: String,
    pub modality: String,
    pub display_name: Option<String>,
    pub is_pinned: bool,
    pub is_ephemeral: bool,
    pub ephemeral_expires_at: Option<i64>,
    pub last_seq: i64,
    /// Wall-clock unix-seconds of the last committed turn. Sidebar
    /// orders by this (recency); zero for never-touched conversations.
    pub last_activity_at: i64,
    /// Channel name (`signal`, `whatsapp`, `email`, ...) for threads
    /// bridged onto a non-web transport. `None` for web-only chats
    /// (Control thread, ad-hoc threads created in the SPA). The
    /// sidebar's "External channels" filter and per-row icon both
    /// key on this — the binding store is the source of truth, not
    /// the conversation `kind` column (which the inbound path stamps
    /// generically, see `chats::ensure_conversation`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_channel: Option<String>,
    /// Bootstrap-icons name (sans `bi-` prefix) the SPA renders next
    /// to the title for bridged threads. Resolved through
    /// `HostTransportRegistry::icon_for(channel)`, which returns the
    /// plugin-manifest-supplied value or the trait-default `"phone"`.
    /// `None` when `transport_channel` is `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_icon: Option<String>,
}

impl From<ThreadSummary> for ThreadSummaryView {
    fn from(s: ThreadSummary) -> Self {
        Self {
            conversation_id: s.conversation_id.as_str().to_owned(),
            kind: s.kind.as_str().to_owned(),
            phase: s.phase.as_str().to_owned(),
            trust_class: s.trust_class,
            modality: s.modality.as_str().to_owned(),
            display_name: s.display_name,
            is_pinned: s.is_pinned,
            is_ephemeral: s.is_ephemeral,
            ephemeral_expires_at: s.ephemeral_expires_at,
            last_seq: s.last_seq.0,
            last_activity_at: s.last_activity_at,
            // Filled in by `list_threads` after a binding lookup;
            // `From` impls don't have DB access, so the conversion
            // produces None and the handler stamps the real values.
            transport_channel: None,
            transport_icon: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ThreadListResponse {
    pub threads: Vec<ThreadSummaryView>,
}

// =====================================================================
// Persisted event payloads
// =====================================================================
//
// These structs are the wire shape `EventLog::commit_turn` / `append`
// rmp-encodes into `event_log.payload_bytes`. They're crate-private
// because no external caller writes / reads them directly; the
// payload encoding is the chats module's contract with the event
// log. Field visibility is `pub(crate)` so `chats.rs` and any future
// chats/* sibling that needs to construct or destructure them works
// without further indirection.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ColdContactPayload {
    pub(crate) text: String,
    pub(crate) sender_principal_id: String,
    pub(crate) approval_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UserMessagePayload {
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sender_principal_id: Option<String>,
    /// Transport this message arrived on. `None` for the default
    /// web path (the SPA falls back to "web" when absent), set to
    /// the bridge name (`signal`, `email`, `voice`, `sms`, ...) for
    /// transport-triggered turns. The SPA reads this off
    /// `MessageView` to render a per-message channel icon so the
    /// operator can tell at a glance "this came in via Signal".
    /// Backward-compatible: existing events without this field
    /// deserialize as `None` and the SPA shows no icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channel_origin: Option<String>,
    /// 2026-05-15 — IDs into `state_attachments` for image attachments
    /// the operator added via the composer's `+` menu. Backward-
    /// compatible default `Vec::new()` so prior events without the
    /// field deserialize cleanly. When non-empty, the chat-history
    /// projection (in `run_real_turn`) fetches each row, base64-
    /// encodes the bytes, and emits the user turn as an OpenAI
    /// vision content array.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) attachment_ids: Vec<String>,
    /// 2026-05-15 — names of skills the operator selected from the
    /// composer's `+` menu for THIS turn. The skill bodies were
    /// already resolved + prepended onto `text` server-side before
    /// the model saw them; this field is purely metadata for the
    /// SPA to render an "applied: foo, bar" chip on the message
    /// bubble (and for forensics — an audit reader can see which
    /// guidance shaped this turn). Backward-compatible default
    /// `Vec::new()` so prior events without the field deserialize.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) applied_skill_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StubModelTurnPayload {
    pub(crate) model: String,
    pub(crate) text: String,
    pub(crate) finish_reason: Option<String>,
    /// Transport the agent's reply went out on (when bridged via a
    /// transport). Same encoding as [`UserMessagePayload::channel_origin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channel_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RealModelTurnPayload {
    pub(crate) model: String,
    pub(crate) text: String,
    pub(crate) finish_reason: Option<String>,
    pub(crate) prompt_tokens: Option<u32>,
    pub(crate) completion_tokens: Option<u32>,
    /// Transport the agent's reply went out on (when bridged via a
    /// transport). Same encoding as [`UserMessagePayload::channel_origin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channel_origin: Option<String>,
}
