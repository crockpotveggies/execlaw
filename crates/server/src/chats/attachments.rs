//! Attachment + message-history helpers for the chats module.
//!
//! Splits out two related concerns:
//!
//!   * **Inbound persistence** — decoding `data:` URLs from the SPA
//!     composer ([`persist_inline_attachments`]) and fetching bytes
//!     out of a transport bridge ([`persist_inbound_attachments`]),
//!     both routing through [`write_attachment_blob`]. The blob
//!     store is content-addressed under `<data_dir>/blobs/`; rows
//!     live in `state_attachments` scoped to the conversation.
//!
//!   * **Outbound shaping** — [`encode_attachments_as_data_urls`]
//!     reverses the persistence for the per-turn vision content
//!     array, plus the small [`extract_*`] / [`hydrate_*`] helpers
//!     that `list_messages` uses to project the on-disk event log
//!     into the SPA's `MessageView` shape.
//!
//! No new types are introduced; all returns either flow into
//! `UserMessagePayload.attachment_ids` (persistence side) or into
//! [`crate::chats::MessageView`] (projection side).

use axum::http::StatusCode;
use execlaw_core::events::{EventKind, EventRecord, ToolResultPayload, ToolUsePayload};
use execlaw_core::ids::ConversationId;

use crate::chats::types::{
    InlineAttachmentRequest, MessageAttachmentView, RealModelTurnPayload, StubModelTurnPayload,
    UserMessagePayload,
};
use crate::state::AppState;

/// Max accepted attachment bytes after base64 decode. ~20 MiB
/// per image, comfortably above what the SPA pre-resizes to (~1 MiB
/// on a 1024px JPEG) but small enough that an accidental "drop a
/// 50 MB raw" doesn't blow up the request body parser or vLLM's
/// per-image budget.
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Allowed mime types for composer-attached images. Anything outside
/// this set fails with `attachment_mime_unsupported` so a malformed
/// SPA upload doesn't end up routed to a vision model that rejects
/// the format. Order doesn't matter — substring `eq` lookup.
pub(crate) const ALLOWED_ATTACHMENT_MIMES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];


/// 2026-05-16 — in-memory representation of an inline attachment
/// after parse/validate but BEFORE any blob or `state_attachments`
/// row is written. The two-phase split lets `send_message` drop
/// (Blocked / UnknownPending parked / Rule-of-Two breach) or 4xx-out
/// a turn without leaving orphan attachment rows or blob files
/// behind. Pre-fix the persistence happened upfront, so a malformed
/// caller could land bytes-on-disk + DB rows in a conversation they
/// weren't authorized to send to.
pub(crate) struct DecodedAttachment {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Phase A: parse + validate every `InlineAttachmentRequest` in the
/// send payload. Returns one `DecodedAttachment` per input on
/// success; on any failure returns the same `ApiError` the legacy
/// `persist_inline_attachments` would have raised so the SPA's per-
/// chip error surfacing is unchanged. NO DB write, NO blob file
/// write happens here — the caller commits with
/// [`commit_decoded_attachments`] after all identity / policy /
/// trust gates pass.
pub(crate) fn decode_inline_attachments(
    requests: &[InlineAttachmentRequest],
) -> Result<Vec<DecodedAttachment>, crate::routes::ApiError> {
    use base64::Engine;

    let mut out = Vec::with_capacity(requests.len());
    for (idx, att) in requests.iter().enumerate() {
        let mime = att.mime.trim().to_lowercase();
        if !ALLOWED_ATTACHMENT_MIMES.contains(&mime.as_str()) {
            return Err(crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_mime_unsupported",
                message: format!(
                    "attachment #{idx}: mime '{}' is not supported (allowed: {})",
                    att.mime,
                    ALLOWED_ATTACHMENT_MIMES.join(", "),
                ),
            });
        }
        // Parse `data:<mime>;base64,<bytes>`. Tolerate optional
        // parameters between the mime and `;base64,` (e.g.
        // `data:image/png;name=foo;base64,...`) since some SPAs add
        // them; we extract the comma-prefix and decode whatever
        // follows.
        let url = att.data_url.as_str();
        let stripped = url
            .strip_prefix("data:")
            .ok_or_else(|| crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_data_url_invalid",
                message: format!("attachment #{idx}: data URL must start with 'data:'"),
            })?;
        let (meta, body) = stripped
            .split_once(',')
            .ok_or_else(|| crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_data_url_invalid",
                message: format!("attachment #{idx}: data URL has no comma separator"),
            })?;
        if !meta.contains("base64") {
            return Err(crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_data_url_invalid",
                message: format!("attachment #{idx}: only base64 data URLs are accepted"),
            });
        }
        let meta_mime = meta.split(';').next().unwrap_or("").trim().to_lowercase();
        if !meta_mime.is_empty() && meta_mime != mime {
            return Err(crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_data_url_invalid",
                message: format!(
                    "attachment #{idx}: mime '{}' in data URL doesn't match declared '{}'",
                    meta_mime, mime
                ),
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .map_err(|e| crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_data_url_invalid",
                message: format!("attachment #{idx}: base64 decode failed: {e}"),
            })?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(crate::routes::ApiError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                code: "attachment_too_large",
                message: format!(
                    "attachment #{idx} is {} bytes (max {})",
                    bytes.len(),
                    MAX_ATTACHMENT_BYTES
                ),
            });
        }
        out.push(DecodedAttachment { mime, bytes });
    }
    Ok(out)
}

/// Phase B: persist every previously-decoded attachment. Writes the
/// content-addressed blob and the `state_attachments` row, returning
/// the fresh ids in input order. Failures during commit (disk full,
/// DB error) flow back as `attachment_write_failed` 500s — by
/// definition all input has already passed validation, so any error
/// here is a server-side problem rather than caller input.
pub(crate) fn commit_decoded_attachments(
    state: &AppState,
    cid: &ConversationId,
    decoded: &[DecodedAttachment],
) -> Result<Vec<String>, crate::routes::ApiError> {
    let mut ids = Vec::with_capacity(decoded.len());
    for (idx, d) in decoded.iter().enumerate() {
        let id = write_attachment_blob(state, cid, &d.mime, &d.bytes).map_err(|e| {
            crate::routes::ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "attachment_write_failed",
                message: format!("attachment #{idx}: {e}"),
            }
        })?;
        ids.push(id);
    }
    Ok(ids)
}

/// Shared core for persisting an attachment's raw bytes. Writes
/// `<data_dir>/blobs/<sha256>` content-addressed (identical bytes
/// share one on-disk file) and inserts a `state_attachments` row
/// scoped to the conversation, returning the fresh attachment id.
///
/// Called from both:
///   * `persist_inline_attachments` — web composer's `+` flow, after
///     decoding the data URL.
///   * `persist_inbound_attachment_bytes` — transport-bridge flow
///     (Signal etc.), after fetching the bytes via the plugin's
///     `<channel>.fetch_attachment` tool.
///
/// Errors are returned as plain strings so callers can wrap them in
/// the right error type for their surface (ApiError for the web
/// path, tracing::warn-and-skip for the inbound path where a single
/// bad attachment shouldn't fail the whole turn).
fn write_attachment_blob(
    state: &AppState,
    cid: &ConversationId,
    mime: &str,
    bytes: &[u8],
) -> Result<String, String> {
    use execlaw_core::attachments::{AttachmentRow, AttachmentStore};
    use execlaw_core::ids::AttachmentId;
    use sha2::{Digest, Sha256};

    let data_dir = state
        .db_config
        .path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let blobs_dir = data_dir.join("blobs");
    std::fs::create_dir_all(&blobs_dir)
        .map_err(|e| format!("create blobs dir {}: {e}", blobs_dir.display()))?;

    let mut h = Sha256::new();
    h.update(bytes);
    let sha = format!("{:x}", h.finalize());
    let path = blobs_dir.join(&sha);
    if !path.exists() {
        std::fs::write(&path, bytes).map_err(|e| format!("write blob {}: {e}", path.display()))?;
    }

    let att_id = AttachmentId::new();
    let row = AttachmentRow {
        id: att_id.clone(),
        conversation_id: cid.clone(),
        mime_type: mime.to_owned(),
        path: path.to_string_lossy().into_owned(),
        sha256: sha,
        received_at: chrono::Utc::now().timestamp(),
    };
    AttachmentStore::new(&state.db)
        .insert(&row)
        .map_err(|e| format!("insert state_attachments row: {e}"))?;
    Ok(att_id.as_str().to_owned())
}

/// Inbound-side: fetch every image attachment on an inbound
/// transport message via the originating channel's
/// `<channel>.fetch_attachment` plugin tool, persist via the same
/// content-addressed `state_attachments` path as the web composer,
/// and return the fresh attachment ids in input order.
///
/// Non-image MIME types are skipped silently (vision models can
/// only see images; PDFs / audio / video would need separate
/// preprocessors). Oversize blobs are rejected per-attachment so a
/// single bad file doesn't kill the rest of the turn. Plugin-tool
/// failures (sidecar offline, network hiccup) are logged at WARN
/// and the failing attachment is dropped — the agent still gets
/// the surviving subset.
pub(crate) async fn persist_inbound_attachments(
    state: &AppState,
    cid: &ConversationId,
    channel: &str,
    attachments: &[execlaw_script::InboundAttachmentMeta],
) -> Vec<String> {
    use base64::Engine;

    if attachments.is_empty() {
        return Vec::new();
    }
    let tool_name = format!("{channel}.fetch_attachment");
    let mut ids = Vec::new();
    for att in attachments {
        // Filter to images upfront — every other media type just
        // wastes a fetch + on-disk blob the LLM can't use.
        let content_type = att.content_type.as_deref().unwrap_or("");
        if !content_type.starts_with("image/") {
            tracing::debug!(
                target: "chats::inbound_attachments",
                bridge_id = %att.bridge_id,
                content_type,
                "non-image inbound attachment skipped (vision-only for now)",
            );
            continue;
        }
        if let Some(size) = att.size_bytes {
            if size as usize > MAX_ATTACHMENT_BYTES {
                tracing::warn!(
                    target: "chats::inbound_attachments",
                    bridge_id = %att.bridge_id,
                    size_bytes = size,
                    max = MAX_ATTACHMENT_BYTES,
                    "inbound attachment exceeds size cap; skipping",
                );
                continue;
            }
        }

        // Call the plugin's fetch_attachment tool. Controller-trust
        // call site (the inbound consumer is host-driven), no
        // capability gate.
        let args = serde_json::json!({"attachment_id": att.bridge_id});
        let resp = match state
            .plugin_host
            .call_tool(&tool_name, args, &["*"], Some("Controller"))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "chats::inbound_attachments",
                    channel,
                    bridge_id = %att.bridge_id,
                    error = %e,
                    "fetch_attachment tool failed; skipping",
                );
                continue;
            }
        };

        // Parse the plugin's response shape:
        //   { data_url: "data:<mime>;base64,...", mime_type, size_bytes }
        let data_url = match resp.get("data_url").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                tracing::warn!(
                    target: "chats::inbound_attachments",
                    channel,
                    bridge_id = %att.bridge_id,
                    "fetch_attachment response missing data_url; skipping",
                );
                continue;
            }
        };
        let reported_mime = resp
            .get("mime_type")
            .and_then(|v| v.as_str())
            .unwrap_or(content_type)
            .to_lowercase();
        // Same parse rules as the web composer's data-URL flow.
        let Some(stripped) = data_url.strip_prefix("data:") else {
            tracing::warn!(
                target: "chats::inbound_attachments",
                bridge_id = %att.bridge_id,
                "fetch_attachment data_url missing 'data:' prefix; skipping",
            );
            continue;
        };
        let Some((meta, body)) = stripped.split_once(',') else {
            tracing::warn!(
                target: "chats::inbound_attachments",
                bridge_id = %att.bridge_id,
                "fetch_attachment data_url missing comma; skipping",
            );
            continue;
        };
        if !meta.contains("base64") {
            tracing::warn!(
                target: "chats::inbound_attachments",
                bridge_id = %att.bridge_id,
                "fetch_attachment data_url is not base64; skipping",
            );
            continue;
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(body.trim()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "chats::inbound_attachments",
                    bridge_id = %att.bridge_id,
                    error = %e,
                    "fetch_attachment base64 decode failed; skipping",
                );
                continue;
            }
        };
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            tracing::warn!(
                target: "chats::inbound_attachments",
                bridge_id = %att.bridge_id,
                size_bytes = bytes.len(),
                max = MAX_ATTACHMENT_BYTES,
                "inbound attachment exceeds size cap after decode; skipping",
            );
            continue;
        }
        // Final mime check — even if the plugin self-reported, only
        // accept what the vision pipeline can ingest.
        if !ALLOWED_ATTACHMENT_MIMES.contains(&reported_mime.as_str()) {
            tracing::debug!(
                target: "chats::inbound_attachments",
                bridge_id = %att.bridge_id,
                mime = %reported_mime,
                "fetched attachment is not an accepted image type; skipping",
            );
            continue;
        }
        match write_attachment_blob(state, cid, &reported_mime, &bytes) {
            Ok(id) => ids.push(id),
            Err(e) => {
                tracing::warn!(
                    target: "chats::inbound_attachments",
                    bridge_id = %att.bridge_id,
                    error = %e,
                    "persist inbound attachment failed; skipping",
                );
            }
        }
    }
    ids
}

/// Load each attachment id from `state_attachments`, read the bytes,
/// and emit a `data:<mime>;base64,<bytes>` URL. Ids missing from the
/// store or pointing at another conversation are skipped silently so
/// a half-broken row can't fail the turn; the agent sees the
/// surviving subset rather than crashing the chat.
///
/// Shared between `run_real_turn` (non-runner path) and
/// `run_runner_turn`. Used to build the OpenAI vision content array
/// that gets sent to the inference backend for the current turn.
pub(crate) fn encode_attachments_as_data_urls(
    db: &execlaw_core::Database,
    cid: &ConversationId,
    attachment_ids: &[String],
) -> Vec<String> {
    use base64::Engine;
    if attachment_ids.is_empty() {
        return Vec::new();
    }
    let store = execlaw_core::attachments::AttachmentStore::new(db);
    let mut out: Vec<String> = Vec::with_capacity(attachment_ids.len());
    for id_str in attachment_ids {
        let id = execlaw_core::ids::AttachmentId::from(id_str.as_str());
        let Ok(Some(row)) = store.get(&id) else {
            tracing::warn!(
                target: "chats::encode_attachments",
                attachment_id = %id_str,
                "attachment row missing — image will not reach the model",
            );
            continue;
        };
        if row.conversation_id.as_str() != cid.as_str() {
            tracing::warn!(
                target: "chats::encode_attachments",
                attachment_id = %id_str,
                "attachment cross-conversation; refusing to include in LLM call",
            );
            continue;
        }
        match std::fs::read(&row.path) {
            Ok(bytes) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                out.push(format!("data:{};base64,{}", row.mime_type, b64));
            }
            Err(e) => {
                tracing::warn!(
                    target: "chats::encode_attachments",
                    attachment_id = %id_str,
                    path = %row.path,
                    error = %e,
                    "attachment blob read failed — skipping",
                );
            }
        }
    }
    out
}

/// Pull the attachment-ids list off a `user_msg` payload. Empty for
/// other kinds and for legacy events that pre-date the field. Used
/// by `list_messages` to surface image refs on the SPA bubble and by
/// the chat-history hydration in `run_real_turn` to encode images as
/// content parts when calling a vision-capable model.
pub(crate) fn extract_attachment_ids(e: &EventRecord) -> Vec<String> {
    match e.kind {
        EventKind::UserMsg => e
            .decode_payload::<UserMessagePayload>()
            .ok()
            .map(|p| p.attachment_ids)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Pull the `applied_skill_names` list off a `user_msg` payload.
/// Empty for other kinds and for legacy events that pre-date the
/// field. Surfaced on `MessageView` so the SPA can render an
/// "applied: foo, bar" chip under the message bubble.
pub(crate) fn extract_applied_skill_names(e: &EventRecord) -> Vec<String> {
    match e.kind {
        EventKind::UserMsg => e
            .decode_payload::<UserMessagePayload>()
            .ok()
            .map(|p| p.applied_skill_names)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Resolve attachment ids → `MessageAttachmentView` rows. Hydrates
/// mime types from `state_attachments`; ids that can't be looked up
/// (deleted blob, cross-conversation probe attempt, DB hiccup) are
/// silently dropped from the response so the SPA renders the
/// best-effort subset rather than failing the whole list call.
pub(crate) fn hydrate_message_attachments(
    db: &execlaw_core::Database,
    cid: &ConversationId,
    ids: &[String],
) -> Vec<MessageAttachmentView> {
    if ids.is_empty() {
        return Vec::new();
    }
    let store = execlaw_core::attachments::AttachmentStore::new(db);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let att_id = execlaw_core::ids::AttachmentId::from(id.as_str());
        match store.get(&att_id) {
            Ok(Some(row)) if row.conversation_id.as_str() == cid.as_str() => {
                out.push(MessageAttachmentView {
                    id: id.clone(),
                    mime: row.mime_type,
                });
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn extract_text(e: &EventRecord) -> Option<String> {
    match e.kind {
        EventKind::UserMsg => e
            .decode_payload::<UserMessagePayload>()
            .ok()
            .map(|p| p.text),
        EventKind::ModelTurn => e
            .decode_payload::<StubModelTurnPayload>()
            .ok()
            .map(|p| p.text)
            .or_else(|| {
                // Fall back to the richer ModelTurnPayload shape produced
                // by the full TurnExecutor.
                e.decode_payload::<RealModelTurnPayload>()
                    .ok()
                    .map(|p| p.text)
            }),
        // 2026-05-15 — surface ToolUse + ToolResult payloads as JSON
        // strings so the SPA's MessageStream can:
        //   * dispatch tool_result events to the chat-component
        //     registry (`detectChatComponent` parses this JSON
        //     looking for `chat_component_kind: "<kind>"`); and
        //   * fall back to a readable `renderToolFallback` for
        //     unknown kinds (better than the empty-text view that
        //     shipped before — was the bug behind "agent ran
        //     chart.render but the chart never appeared").
        //
        // For ToolResult, prefer the inner Ok(...) value when
        // success — that's the JSON the tool actually emitted (and
        // what the chat-component dispatcher needs). Errors get
        // wrapped in a small envelope so the SPA's fallback shows
        // "tool failed: <reason>" rather than dumping the raw Result
        // discriminant.
        EventKind::ToolUse => e
            .decode_payload::<ToolUsePayload>()
            .ok()
            .and_then(|p| serde_json::to_string(&p.args_json).ok()),
        EventKind::ToolResult => e
            .decode_payload::<ToolResultPayload>()
            .ok()
            .and_then(|p| match p.outcome {
                Ok(value) => serde_json::to_string(&value).ok(),
                Err(reason) => serde_json::to_string(&serde_json::json!({
                    "error": reason,
                }))
                .ok(),
            }),
        _ => None,
    }
}

/// Pull `channel_origin` out of the payload for events that carry
/// it (user_msg + model_turn). Returns None for other event kinds
/// or when the field is absent (legacy events / web-originated
/// turns). Surfaced on `MessageView` so the SPA can render a
/// per-message transport icon.
pub(crate) fn extract_channel_origin(e: &EventRecord) -> Option<String> {
    match e.kind {
        EventKind::UserMsg => e
            .decode_payload::<UserMessagePayload>()
            .ok()
            .and_then(|p| p.channel_origin),
        EventKind::ModelTurn => e
            .decode_payload::<RealModelTurnPayload>()
            .ok()
            .and_then(|p| p.channel_origin)
            .or_else(|| {
                e.decode_payload::<StubModelTurnPayload>()
                    .ok()
                    .and_then(|p| p.channel_origin)
            }),
        _ => None,
    }
}

// =====================================================================
// Data refs (2026-05-16)
// =====================================================================
//
// A "data ref" is the host's mechanism for letting one tool's output
// flow into another tool's input WITHOUT the model having to re-emit
// the bytes. The chart-from-stock-history use case made the cost
// concrete: the model emitting 125 NKE OHLC rows back into a
// `chart.render` call took 120+ seconds of decode and timed out the
// HTTP read at vLLM. The right answer is to never make the model
// retype data it just received — store the value server-side under a
// fresh id, give the model the id, and let it pass that id to the
// next tool.
//
// Storage: piggybacks on `state_artifacts` with mime
// `application/json` and `kind = "plugin_artifact"`. Inherits the
// table's TTL + content-addressing for free.
//
// Resolution: see `tool_dispatch::resolve_data_refs` for the wire
// shape (`{"$data_ref": "<id>"}`) and the recursive substitution
// pass that runs before every tool invocation.

/// Default TTL for data refs — long enough that a multi-round turn
/// (tool A produces a ref → model reasons → tool B consumes it) has
/// breathing room, short enough that abandoned refs don't pile up
/// indefinitely on disk. The ephemeral sweeper culls expired rows
/// + their on-disk bytes on its normal interval.
pub(crate) const DATA_REF_DEFAULT_TTL_SECS: i64 = 60 * 60; // 1h

/// Hard cap on a single data ref's JSON payload. Larger than the
/// 10 MiB attachment cap because the use case (entire historical
/// candle series, deep-research bibliographies, large search result
/// sets) trends bigger than one image; small enough that a runaway
/// plugin can't fill the artifacts dir with one tool call.
pub(crate) const DATA_REF_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Persist a JSON value as a data ref, returning the fresh attachment
/// id. The id is a UUID stamped on `state_artifacts.id`; the bytes
/// land in the same on-disk artifacts root as plugin-rendered chart
/// PNGs etc. `ttl_seconds=None` uses [`DATA_REF_DEFAULT_TTL_SECS`].
pub(crate) fn persist_data_ref(
    state: &AppState,
    plugin_id: &str,
    value: &serde_json::Value,
    ttl_seconds: Option<i64>,
) -> Result<execlaw_core::attachments::PluginArtifactCreated, String> {
    use execlaw_core::attachments::AttachmentStore;

    let bytes = serde_json::to_vec(value).map_err(|e| format!("data_ref encode: {e}"))?;
    if bytes.len() > DATA_REF_MAX_BYTES {
        return Err(format!(
            "data_ref payload {} bytes exceeds cap {}",
            bytes.len(),
            DATA_REF_MAX_BYTES
        ));
    }
    let ttl = ttl_seconds.or(Some(DATA_REF_DEFAULT_TTL_SECS));
    let now = chrono::Utc::now().timestamp();
    let root = crate::host_caps_impl::builtin_artifacts_root_path();
    let store = AttachmentStore::new(&state.db);
    store
        .insert_plugin_artifact(
            &root,
            plugin_id,
            "data_ref.json",
            "application/json",
            &bytes,
            ttl,
            now,
        )
        .map_err(|e| format!("data_ref persist: {e}"))
}

/// Look up a data ref by id and parse its on-disk bytes as JSON.
///
/// Errors:
///   * `data_ref '<id>' not found` — no `state_artifacts` row.
///   * `data_ref '<id>' mime is '<mime>'` — the row exists but isn't a
///     JSON ref (e.g. operator tried to point a `$data_ref` at a PNG
///     chart attachment id).
///   * `data_ref '<id>' expired` — the row's `expires_at` is past.
///   * read / decode errors propagate verbatim.
pub(crate) fn fetch_data_ref(
    db: &execlaw_core::Database,
    attachment_id: &str,
) -> Result<serde_json::Value, String> {
    use execlaw_core::attachments::AttachmentStore;
    let store = AttachmentStore::new(db);
    let row = store
        .get_artifact(attachment_id)
        .map_err(|e| format!("data_ref lookup: {e}"))?
        .ok_or_else(|| format!("data_ref '{attachment_id}' not found"))?;
    if row.mime_type != "application/json" {
        return Err(format!(
            "data_ref '{attachment_id}' mime is '{}' (expected application/json)",
            row.mime_type
        ));
    }
    if let Some(expires_at) = row.expires_at {
        let now = chrono::Utc::now().timestamp();
        if now > expires_at {
            return Err(format!(
                "data_ref '{attachment_id}' expired {}s ago",
                now - expires_at
            ));
        }
    }
    let bytes =
        std::fs::read(&row.path).map_err(|e| format!("data_ref read {}: {e}", row.path))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("data_ref decode: {e}"))
}
