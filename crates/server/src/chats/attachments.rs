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
/// per-image budget. Also applies to non-image attachments (CSV,
/// JSON, etc.) routed to the python-sandbox kernel via Phase 3
/// hydration.
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Allowed mime types for composer-attached files. Originally
/// images-only for the vision-content path; expanded 2026-05-18 to
/// cover the "feed this to pandas / read this PDF" cases the
/// python-sandbox plugin enables. Order doesn't matter — substring
/// `eq` lookup against the lowercased declared mime.
///
/// Splits conceptually into two groups (no enforced split; the
/// down-stream code paths handle each correctly):
///
///   * **Image MIMEs** → land as vision content parts on the LLM
///     request, just like before. Agent "sees" them directly.
///   * **Data MIMEs** → land only on disk in `state_attachments` and
///     get hydrated to `/work/<convo_id>/uploads/<filename>` by
///     the python-sandbox sidecar (Phase 3). Agent learns about
///     them via the per-turn `[Attached files: …]` context block
///     (Phase C of this work) so it knows to call `python.execute`
///     with pandas / pdf-readers / etc.
///
/// **NOT** included by design: executables (`.exe`, `.dll`, `.sh`,
/// `.bat`), source code (`.py`, `.js`) — the v1 scope is "data the
/// agent operates ON", not "scripts the agent runs". Sandbox
/// isolation would technically contain a bad script, but exposing
/// the upload pipe to executables makes the threat model harder to
/// reason about. Operators who really need this can land it as a
/// follow-up with explicit opt-in.
pub(crate) const ALLOWED_ATTACHMENT_MIMES: &[&str] = &[
    // Image family (vision-content path).
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    // Tabular / structured data (pandas-friendly).
    "text/csv",
    "text/tab-separated-values",
    "application/json",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", // .xlsx
    "application/vnd.ms-excel",                                          // .xls
    // Plain text + markdown (readable by python.execute via open()).
    "text/plain",
    "text/markdown",
    // Documents.
    "application/pdf",
];

/// `true` if the mime is in the image group — used by callers that
/// need to fan out only images to the vision-content path while
/// non-image attachments stay on disk for python-sandbox hydration.
pub(crate) fn is_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

/// 2026-05-16 — in-memory representation of an inline attachment
/// after parse/validate but BEFORE any blob or `state_attachments`
/// row is written. The two-phase split lets `send_message` drop
/// (Blocked / UnknownPending parked / Rule-of-Two breach) or 4xx-out
/// a turn without leaving orphan attachment rows or blob files
/// behind. Pre-fix the persistence happened upfront, so a malformed
/// caller could land bytes-on-disk + DB rows in a conversation they
/// weren't authorized to send to.
#[cfg_attr(test, derive(Debug))]
pub(crate) struct DecodedAttachment {
    pub mime: String,
    pub bytes: Vec<u8>,
    /// 2026-05-18 — operator-facing original filename from the SPA
    /// file picker (`InlineAttachmentRequest.filename`). Required
    /// for non-image MIMEs so hydration can write the blob to
    /// `/work/<convo>/uploads/<filename>` under the original name
    /// (the agent references it by name in `python.execute`). For
    /// images it's optional — they land in vision content where
    /// the filename isn't user-visible.
    pub filename: Option<String>,
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
        // Non-image MIMEs MUST carry a filename — hydration needs
        // it to write the blob to the kernel's `/work/<convo>/
        // uploads/<filename>` mount under the operator's chosen
        // name. Images can skip it (vision content doesn't surface
        // filenames). Sanitize away path separators on the way
        // through so a malicious or sloppy payload can't write to
        // `/work/<convo>/uploads/../../etc/passwd` (hydration also
        // re-checks, but defense in depth).
        let filename = att.filename.as_ref().map(|s| sanitize_filename(s));
        if !is_image_mime(&mime) && filename.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
            return Err(crate::routes::ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "attachment_filename_required",
                message: format!(
                    "attachment #{idx}: filename is required for non-image MIME '{mime}' \
                     so the python-sandbox sidecar can hydrate the file under its \
                     original name"
                ),
            });
        }
        out.push(DecodedAttachment {
            mime,
            bytes,
            filename,
        });
    }
    Ok(out)
}

/// Strip path separators + leading dots from an operator-supplied
/// filename so a hostile or careless payload can't traverse out of
/// `/work/<convo>/uploads/`. Returns the file's basename, with
/// `/` and `\` collapsed to the trailing component, a leading
/// `.` (hidden-file marker on Unix) trimmed, and the result capped
/// at 255 bytes — the filename limit on most POSIX filesystems and
/// the Windows MAX_PATH segment cap. Empty / all-dots input
/// returns the empty string; the caller treats that as "missing
/// filename" and rejects.
///
/// Intentionally minimal — full canonicalization happens in the
/// hydration path which also re-joins through `Path::join` against
/// a canonicalized work root. This is the first-line filter so the
/// SPA's chip + agent context block don't show a path-shaped name.
fn sanitize_filename(raw: &str) -> String {
    /// Common FS segment cap on both Linux (ext4 / btrfs / xfs all
    /// at 255) and Windows (MAX_PATH segment). Truncation preserves
    /// the extension when possible so the MIME-vs-extension match
    /// at the python kernel still works.
    const MAX_FILENAME_BYTES: usize = 255;

    let trimmed = raw.trim();
    // Take whatever's after the last separator.
    let last_sep = trimmed
        .rfind(|c: char| c == '/' || c == '\\')
        .map(|i| i + 1)
        .unwrap_or(0);
    let basename = &trimmed[last_sep..];
    // Strip leading dots so "..\foo" doesn't survive as "..foo"
    // after sep-stripping a path like `foo\..\bar`.
    let stripped = basename.trim_start_matches('.');

    if stripped.len() <= MAX_FILENAME_BYTES {
        return stripped.to_owned();
    }
    // Truncate at the byte cap, preserving any extension (last
    // `.<chars>`) so pandas' MIME-by-extension routing still works.
    // Extension preservation is best-effort: if the extension is
    // itself wildly long (>16 bytes), give up and hard-truncate —
    // the operator gets a usable-shaped name, not a perfect one.
    let extension = stripped.rsplit_once('.').and_then(|(_, ext)| {
        if !ext.is_empty() && ext.len() <= 16 {
            Some(ext)
        } else {
            None
        }
    });
    match extension {
        Some(ext) => {
            // Reserve space for ".<ext>" + nul-margin.
            let dot_ext_len = ext.len() + 1;
            let stem_budget = MAX_FILENAME_BYTES.saturating_sub(dot_ext_len);
            // `stripped` is guaranteed to start with the stem; slice
            // at a UTF-8 char boundary to avoid panicking on multibyte
            // chars in the middle of an oversize name.
            let mut stem_end = stem_budget.min(stripped.len() - dot_ext_len);
            while !stripped.is_char_boundary(stem_end) && stem_end > 0 {
                stem_end -= 1;
            }
            format!("{}.{}", &stripped[..stem_end], ext)
        }
        None => {
            let mut end = MAX_FILENAME_BYTES;
            while !stripped.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            stripped[..end].to_owned()
        }
    }
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
    use execlaw_core::attachments::AttachmentStore;

    // 2026-05-18 — suffix-on-conflict (audit-pass follow-up).
    // Before persisting, build the set of already-used filenames
    // on this conversation so colliding uploads land as
    // `data (1).csv` rather than overwriting the first blob on
    // disk during Phase-3 hydration. Without this fix, two
    // uploads of `data.csv` would (a) leave the older row
    // orphaned (no on-disk file), (b) tell the agent two files
    // exist via the prose block's dedupe behavior — wait, the
    // prose dedupes to ONE bullet, which is correct, but the
    // operator legitimately attached two different files and one
    // is silently lost. Better to keep both.
    //
    // Pre-existing filenames sourced from state_attachments;
    // in-payload duplicates resolved against the running set as
    // we walk `decoded` so two files named `data.csv` picked in
    // a single send land as `data.csv` + `data (1).csv`.
    let mut used: std::collections::HashSet<String> = AttachmentStore::new(&state.db)
        .list_for_conversation(cid)
        .map_err(|e| crate::routes::ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "attachment_write_failed",
            message: format!("list existing attachments for collision check: {e}"),
        })?
        .into_iter()
        .filter_map(|r| r.filename)
        .collect();

    let mut ids = Vec::with_capacity(decoded.len());
    for (idx, d) in decoded.iter().enumerate() {
        // For images the filename is optional (vision content
        // doesn't surface a name to the model). When absent, skip
        // the collision check entirely — `write_attachment_blob`
        // writes a content-addressed blob with no name.
        let persisted_name = match d.filename.as_deref() {
            Some(requested) if !requested.is_empty() => {
                let resolved = unique_filename(requested, &used);
                used.insert(resolved.clone());
                Some(resolved)
            }
            _ => None,
        };
        let id = write_attachment_blob(state, cid, &d.mime, &d.bytes, persisted_name.as_deref())
            .map_err(|e| crate::routes::ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "attachment_write_failed",
                message: format!("attachment #{idx}: {e}"),
            })?;
        ids.push(id);
    }
    Ok(ids)
}

/// Resolve a filename collision by appending ` (N)` before the
/// extension. Mirrors how every major OS file picker resolves
/// the same case ("data.csv" → "data (1).csv" → "data (2).csv").
///
/// Preserves the extension (last `.<ext>` of ≤16 bytes) so the
/// MIME-by-extension routing still works after the suffix. If
/// the base has no recognizable extension, the counter goes at
/// the very end (`notes` → `notes (1)`).
///
/// Bounded at 1000 iterations as a safety net — at that point
/// something else is wrong (1000 collisions would imply a
/// near-exact-name spammer or a runaway loop) and we fail loud
/// rather than block forever. Returns the un-suffixed name as a
/// fallback so the caller still proceeds; the agent will see
/// the dedupe-in-prose code (gap 5 fix) kick in as a second line
/// of defense.
fn unique_filename(base: &str, used: &std::collections::HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_owned();
    }
    // Split into stem + extension. Same rule as sanitize_filename's
    // truncation path: extension is the trailing `.<chars>` of
    // ≤16 bytes, anything longer is treated as opaque suffix.
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !e.is_empty() && e.len() <= 16 => (s, Some(e)),
        _ => (base, None),
    };
    for n in 1..=1000 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    tracing::warn!(
        target: "chats::commit_decoded_attachments",
        base = %base,
        "filename collision counter exhausted at 1000; falling back to un-suffixed name. \
         The hydration overwrite + prose dedupe will smooth this over but the operator may \
         see a file silently replaced — escalate if this triggers in practice."
    );
    base.to_owned()
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
    filename: Option<&str>,
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
        // 2026-05-18 — populated from the SPA composer's file picker
        // (`InlineAttachmentRequest.filename`) for non-image MIMEs so
        // python-sandbox hydration can land the file at
        // `/work/<convo>/uploads/<filename>`. Sanitized at decode
        // (path separators stripped, leading dots trimmed) before
        // reaching here. None for images and for inbound-transport
        // attachments that don't carry an original filename.
        filename: filename.map(|s| s.to_owned()),
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
        // Inbound transport attachments today are image-only (filter
        // at the top of this fn ensures that). No filename is carried
        // by the transport metas, so `filename=None` here — the agent
        // sees these as vision content, not as files to operate on.
        // If a future transport surfaces non-image attachments + a
        // filename, plumb `att.filename` (or equivalent) here.
        match write_attachment_blob(state, cid, &reported_mime, &bytes, None) {
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
        // 2026-05-18 — CRITICAL: only IMAGE mime types ride the
        // vision-content path. Non-image attachments (CSV / PDF /
        // JSON / etc.) live on disk and the agent learns about
        // them via `build_attached_files_block`'s prose context
        // (Phase C). Encoding a CSV as a `data:text/csv;base64,...`
        // and shoving it into the OpenAI vision array fails at
        // the inference backend with:
        //   Failed to load image: cannot identify image file
        //   <_io.BytesIO object>
        // — and the operator's "summarize this csv" turn 500s.
        // This filter was missing in the original encoder; every
        // attachment was treated as an image.
        if !is_image_mime(&row.mime_type) {
            tracing::debug!(
                target: "chats::encode_attachments",
                attachment_id = %id_str,
                mime = %row.mime_type,
                "skipping non-image attachment for vision-content array — \
                 agent will reach the file via python.execute against \
                 /work/uploads/",
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

/// 2026-05-18 — assemble the per-turn "Attached files" prose block
/// the agent sees alongside the time / trust / channel context.
/// Lists every NON-image attachment on the conversation (images
/// land as vision content parts and don't need a prose mention),
/// with the path they appear at inside the python-sandbox kernel
/// so the agent can call `python.execute` against them by name.
///
/// Returns `None` when the conversation has no non-image
/// attachments — the caller skips appending so the turn prompt
/// doesn't grow an empty section.
///
/// Why conversation-scoped (not turn-scoped): attachments uploaded
/// on turn N persist in `state_attachments` + on disk for the
/// whole conversation. On turn N+1 ("make a chart from that csv")
/// the file is still accessible in `/work/uploads/`, so the agent
/// needs to keep being reminded it exists.
pub(crate) fn build_attached_files_block(state: &AppState, cid: &ConversationId) -> Option<String> {
    use execlaw_core::attachments::AttachmentStore;
    use execlaw_core::conversation::ConversationStore;

    // Fast path (migration 0014). The vast majority of turns belong
    // to conversations that have never had a non-image attachment;
    // the per-conversation `has_attachments` flag short-circuits
    // those without going through the full
    // `list_for_conversation` scan + row decode. A DB error here is
    // a soft failure — fall through to the slow path so a transient
    // SQLite error doesn't silently drop a real attachment from the
    // agent's view.
    match ConversationStore::new(&state.db).has_attachments(cid) {
        Ok(false) => return None,
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(
                target: "chats::attached_files_block",
                conversation_id = %cid.as_str(),
                error = %e,
                "has_attachments probe failed; falling through to full list_for_conversation",
            );
        }
    }
    let rows = match AttachmentStore::new(&state.db).list_for_conversation(cid) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "chats::attached_files_block",
                conversation_id = %cid.as_str(),
                error = %e,
                "list_for_conversation failed; agent will not be told about attached files this turn",
            );
            return None;
        }
    };
    let python_available = state
        .plugin_host
        .registry()
        .lookup_any("python.execute")
        .is_some();
    format_attached_files_block(&rows, python_available, Some(cid.as_str()))
}

/// Pure formatter for the attached-files prose block. Separated
/// from [`build_attached_files_block`] so unit tests can exercise
/// every branch (image filter, python-unavailable gate, filename
/// fallback) without standing up a full `AppState` + tokio runtime.
/// The wrapper's only job is to source the inputs from live state.
///
/// `python_available` reflects whether `python.execute` is in the
/// runtime tool catalog. When false, returns None — telling the
/// agent files are at `/work/uploads/` when python-sandbox isn't
/// installed would be a lie (hydration only runs with the sidecar
/// healthy).
///
/// `cid_for_log` is only used for the debug-log breadcrumb when
/// the python-unavailable gate fires; `None` skips the log (test
/// path).
pub(crate) fn format_attached_files_block(
    rows: &[execlaw_core::attachments::AttachmentRow],
    python_available: bool,
    cid_for_log: Option<&str>,
) -> Option<String> {
    let non_image: Vec<&execlaw_core::attachments::AttachmentRow> = rows
        .iter()
        .filter(|r| !is_image_mime(&r.mime_type))
        .collect();
    if non_image.is_empty() {
        return None;
    }
    // 2026-05-18 — dedupe by hydrated filename. If the operator
    // attaches `data.csv` on turn 1 + `data.csv` again on turn 3
    // (different content, both sha256-content-addressed in the
    // blob store), hydration writes /work/uploads/data.csv with
    // the latter blob's bytes — the first is silently
    // overwritten. Listing both rows in the prose would tell the
    // agent two files exist when only one does, which the agent
    // catches mid-turn ("FileNotFoundError: data (1).csv") and
    // gets confused. Keep the most-recent row per filename
    // (rows are returned ordered by received_at ASC; iterate +
    // overwrite into a BTreeMap keyed by name).
    use std::collections::BTreeMap;
    let mut latest_by_name: BTreeMap<String, &execlaw_core::attachments::AttachmentRow> =
        BTreeMap::new();
    for r in &non_image {
        let display_name = match r.filename.as_deref() {
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => crate::python_sandbox::service::derive_default_filename(&r.mime_type, &r.sha256),
        };
        latest_by_name.insert(display_name, *r);
    }
    // Always emit the block when non-image attachments exist — the
    // attachment-ref contract is: surface every attached file to
    // the agent and let the agent decide what to do with it. The
    // wording adapts to whether `python.execute` is available so
    // the agent knows which read path it has, but absence of the
    // sandbox NEVER causes a silent drop. Modern multimodal models
    // can natively process PDFs / read pasted contents / ask for
    // clarification; gaslighting the model with "no file" because
    // the host doesn't have one specific tool installed is the
    // wrong contract.
    let _ = cid_for_log; // logging not needed in the always-emit branch
    let mut out = String::from(
        "## Attached files\n\n\
         The operator has attached the following files to this conversation. \
         Acknowledge them when the operator asks; do not claim the files \
         don't exist.\n\n",
    );
    // Iterate the deduped map. Order doesn't matter to the agent
    // (the model uses the filenames, not their position), and
    // BTreeMap gives stable alphabetical order so the same set of
    // attachments produces the same prose across turns — useful
    // for KV-cache reuse.
    for (display_name, r) in &latest_by_name {
        let size = std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0);
        if python_available {
            out.push_str(&format!(
                "* `{name}` ({mime}, {size} bytes) — available at \
                 `/work/uploads/{name}` inside `python.execute`. Read the \
                 file with `open()` / `pandas.read_csv()` / `PyPDF2.PdfReader()` \
                 / etc. as appropriate.\n",
                name = display_name,
                mime = r.mime_type,
                size = size,
            ));
        } else {
            out.push_str(&format!(
                "* `{name}` ({mime}, {size} bytes) — the file is attached but \
                 no sandbox tool is currently installed for the host to read \
                 it on your behalf. If the operator asks about its contents, \
                 (a) describe what you can infer from the filename + mime, \
                 (b) ask the operator to paste the relevant excerpt, or \
                 (c) note that enabling the python sandbox in Settings would \
                 let you read it directly.\n",
                name = display_name,
                mime = r.mime_type,
                size = size,
            ));
        }
    }
    Some(out)
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
                // Fallback to `derive_default_filename` for legacy
                // rows so the SPA chip still has SOMETHING to show.
                // Matches the prose-block fallback so the agent's
                // "Attached files" view + the user's message chip
                // show the same name.
                let filename = row.filename.clone().or_else(|| {
                    Some(crate::python_sandbox::service::derive_default_filename(
                        &row.mime_type,
                        &row.sha256,
                    ))
                });
                let size_bytes = std::fs::metadata(&row.path).map(|m| m.len()).unwrap_or(0);
                out.push(MessageAttachmentView {
                    id: id.clone(),
                    mime: row.mime_type,
                    filename,
                    size_bytes,
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

// The constants + `persist_data_ref` are the producer half of the
// data-ref scaffolding; only the consumer half (`fetch_data_ref` +
// the `{"$data_ref": "<id>"}` substitution pass in `tool_dispatch`)
// is wired into the agent loop today. Plugins that want to mint a
// data ref still go through their own AttachmentStore call directly.
// Kept around so the producer side is ready when a tool surfaces
// that needs it — flag as allowed-dead-code rather than deleting so
// the doc + invariants stay where future callers will look for them.

/// Default TTL for data refs — long enough that a multi-round turn
/// (tool A produces a ref → model reasons → tool B consumes it) has
/// breathing room, short enough that abandoned refs don't pile up
/// indefinitely on disk. The ephemeral sweeper culls expired rows
/// + their on-disk bytes on its normal interval.
#[allow(dead_code)]
pub(crate) const DATA_REF_DEFAULT_TTL_SECS: i64 = 60 * 60; // 1h

/// Hard cap on a single data ref's JSON payload. Larger than the
/// 10 MiB attachment cap because the use case (entire historical
/// candle series, deep-research bibliographies, large search result
/// sets) trends bigger than one image; small enough that a runaway
/// plugin can't fill the artifacts dir with one tool call.
#[allow(dead_code)]
pub(crate) const DATA_REF_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Persist a JSON value as a data ref, returning the fresh attachment
/// id. The id is a UUID stamped on `state_artifacts.id`; the bytes
/// land in the same on-disk artifacts root as plugin-rendered chart
/// PNGs etc. `ttl_seconds=None` uses [`DATA_REF_DEFAULT_TTL_SECS`].
#[allow(dead_code)]
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
    let bytes = std::fs::read(&row.path).map_err(|e| format!("data_ref read {}: {e}", row.path))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("data_ref decode: {e}"))
}

#[cfg(test)]
mod tests {
    //! Unit tests for the MIME-widening + filename-passthrough work
    //! that landed alongside the python-sandbox attach-file UX
    //! (2026-05-18). These exercise the parse/validate layer only;
    //! the persistence layer is covered by integration tests in
    //! `crates/server/tests/chats_*.rs` and the e2e SPA-to-disk path
    //! lives in `crates/server/src/python_sandbox/hydration.rs`.
    //!
    //! Coverage rationale (see project_extensive_testing axiom #13):
    //!   * every newly-accepted MIME has a happy-path test
    //!   * every newly-rejected MIME (executables, source code) has
    //!     a rejection test
    //!   * filename sanitization covers the path-traversal attack
    //!     surface end-to-end
    //!   * filename-required-for-non-images gates the contract that
    //!     hydration relies on

    use super::*;
    use crate::chats::types::InlineAttachmentRequest;
    use base64::Engine;

    fn req(mime: &str, body: &[u8], filename: Option<&str>) -> InlineAttachmentRequest {
        let b64 = base64::engine::general_purpose::STANDARD.encode(body);
        InlineAttachmentRequest {
            mime: mime.to_owned(),
            data_url: format!("data:{mime};base64,{b64}"),
            filename: filename.map(|s| s.to_owned()),
        }
    }

    #[test]
    fn accepts_every_image_mime_with_no_filename() {
        for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
            let r = req(mime, b"fake-image-bytes", None);
            let out = decode_inline_attachments(&[r]).unwrap_or_else(|e| {
                panic!("image mime {mime} rejected: {}", e.message);
            });
            assert_eq!(out.len(), 1, "mime {mime}");
            assert_eq!(out[0].mime, mime, "mime preserved");
            assert!(
                out[0].filename.is_none(),
                "image mime {mime} doesn't require filename"
            );
        }
    }

    #[test]
    fn accepts_every_data_mime_with_filename() {
        for (mime, fname) in [
            ("text/csv", "data.csv"),
            ("text/tab-separated-values", "data.tsv"),
            ("application/json", "config.json"),
            ("text/plain", "notes.txt"),
            ("text/markdown", "readme.md"),
            ("application/pdf", "report.pdf"),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "book.xlsx",
            ),
            ("application/vnd.ms-excel", "legacy.xls"),
        ] {
            let r = req(mime, b"placeholder-bytes", Some(fname));
            let out = decode_inline_attachments(&[r]).unwrap_or_else(|e| {
                panic!("data mime {mime} rejected: {}", e.message);
            });
            assert_eq!(out.len(), 1, "mime {mime}");
            assert_eq!(out[0].filename.as_deref(), Some(fname));
        }
    }

    #[test]
    fn rejects_executable_mimes() {
        for mime in [
            "application/x-msdownload",  // .exe
            "application/x-sh",          // .sh
            "application/javascript",    // .js
            "application/x-python-code", // .pyc
            "application/x-executable",
        ] {
            let r = req(mime, b"...", Some("payload"));
            let err = decode_inline_attachments(&[r])
                .expect_err(&format!("executable mime {mime} should have been rejected"));
            assert_eq!(err.code, "attachment_mime_unsupported");
        }
    }

    #[test]
    fn rejects_data_mime_without_filename() {
        let r = req("text/csv", b"a,b\n1,2\n", None);
        let err = decode_inline_attachments(&[r]).expect_err("missing filename should reject");
        assert_eq!(err.code, "attachment_filename_required");
    }

    #[test]
    fn rejects_data_mime_with_empty_filename() {
        let r = req("text/csv", b"a,b\n1,2\n", Some("   "));
        let err = decode_inline_attachments(&[r]).expect_err("blank filename should reject");
        assert_eq!(err.code, "attachment_filename_required");
    }

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("\\windows\\system32\\bad.exe"), "bad.exe");
        assert_eq!(sanitize_filename("../../etc/shadow"), "shadow");
        assert_eq!(sanitize_filename("clean.csv"), "clean.csv");
        // Leading dots stripped so "..\\foo" → "foo" via combined behaviour.
        assert_eq!(sanitize_filename("..foo.csv"), "foo.csv");
        // Whitespace trimmed.
        assert_eq!(sanitize_filename("  spaced.txt  "), "spaced.txt");
    }

    #[test]
    fn decoded_filename_is_sanitized() {
        // A hostile payload tries to escape /work/<convo>/uploads/
        // by claiming the filename is `../../etc/passwd`. The
        // decoder strips path separators before the value reaches
        // hydration.
        let r = req("text/csv", b"a,b\n", Some("../../etc/passwd"));
        let out = decode_inline_attachments(&[r]).unwrap();
        assert_eq!(out[0].filename.as_deref(), Some("passwd"));
    }

    #[test]
    fn is_image_mime_matches_only_the_four_image_types() {
        assert!(is_image_mime("image/png"));
        assert!(is_image_mime("image/jpeg"));
        assert!(is_image_mime("image/webp"));
        assert!(is_image_mime("image/gif"));
        assert!(!is_image_mime("text/csv"));
        assert!(!is_image_mime("application/pdf"));
        assert!(!is_image_mime("image/svg+xml")); // intentionally not in v1
        assert!(!is_image_mime(""));
    }

    #[test]
    fn oversize_attachment_is_rejected_regardless_of_mime() {
        let big = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        let r = req("text/csv", &big, Some("big.csv"));
        let err = decode_inline_attachments(&[r]).expect_err("oversize should reject");
        assert_eq!(err.code, "attachment_too_large");
    }

    /// Phase C: the per-turn "Attached files" prose block lives in
    /// `build_attached_files_block`. We can't easily mock the
    /// `AppState` it consumes, so the test exercises the formatting
    /// helpers + the static text expectations the routing prose
    /// relies on. The behavior of "list_for_conversation skips
    /// images and returns non-image rows" is covered by the
    /// `AttachmentStore` tests in `crates/core`.

    #[test]
    fn routing_prose_advertises_python_when_python_tools_present() {
        // When the python-sandbox plugin is installed, the routing
        // prose should include the `python` family guidance so the
        // model knows to reach for it (not just inherit the generic
        // "plugin-prefixed tool" line).
        let routing = crate::chats::build_tool_routing_prose(
            &[],
            &[
                "python.execute".to_owned(),
                "python.reset".to_owned(),
                "python.interrupt".to_owned(),
                "python.list_files".to_owned(),
            ],
        );
        assert!(
            routing.contains("python.execute"),
            "routing prose should mention python.execute when python.* tools registered:\n\n{routing}",
        );
        assert!(
            routing.contains("/work/uploads/"),
            "routing prose should mention the uploads path so the agent knows where attached files live:\n\n{routing}",
        );
    }

    #[test]
    fn routing_prose_omits_python_when_python_tools_absent() {
        // Other plugins shouldn't see the python guidance — it'd
        // mislead the model into looking for tools it doesn't have.
        let routing =
            crate::chats::build_tool_routing_prose(&[], &["signal.send_message".to_owned()]);
        assert!(
            !routing.contains("python.execute"),
            "routing prose must NOT mention python.execute when python.* tools are absent:\n\n{routing}",
        );
    }

    // ------------------------------------------------------------
    // Phase C audit-pass tests (2026-05-18). Cover the formatting
    // function, filename length cap, and the cross-module name
    // contract with the hydration helper.
    // ------------------------------------------------------------

    use execlaw_core::attachments::AttachmentRow;
    use execlaw_core::ids::{AttachmentId, ConversationId};

    fn row(filename: Option<&str>, mime: &str, sha: &str) -> AttachmentRow {
        AttachmentRow {
            id: AttachmentId::new(),
            conversation_id: ConversationId::new(),
            mime_type: mime.to_owned(),
            // Path that almost certainly doesn't exist on disk —
            // size lookup falls through to 0 via the `.unwrap_or(0)`
            // in `format_attached_files_block`, which is fine for
            // pure-format tests (we assert text shape, not byte
            // counts).
            path: format!("nonexistent-blob-{sha}"),
            sha256: sha.to_owned(),
            received_at: 0,
            filename: filename.map(|s| s.to_owned()),
        }
    }

    #[test]
    fn format_block_returns_none_when_no_non_image_rows() {
        let rows = [
            row(None, "image/png", "shapng"),
            row(None, "image/jpeg", "shajpg"),
        ];
        assert!(
            format_attached_files_block(&rows, true, None).is_none(),
            "block should be None when all rows are images",
        );
    }

    #[test]
    fn format_block_always_emitted_when_attachments_exist_regardless_of_python() {
        // Attachment-ref contract: the agent ALWAYS learns about
        // every attached file. Tooling availability shapes the
        // prose (read paths the agent has vs. doesn't have), but
        // never silently drops attachments — gaslighting the agent
        // with "no file" because one specific tool isn't installed
        // is the wrong contract, especially for multimodal models
        // that can natively process documents.
        let rows = [row(Some("data.csv"), "text/csv", "sha1")];
        let block = format_attached_files_block(&rows, false, None)
            .expect("block must emit even when python.execute is unavailable");
        assert!(
            block.contains("data.csv"),
            "block must name the file so the agent can acknowledge it:\n\n{block}",
        );
        assert!(
            !block.contains("/work/uploads/"),
            "when python.execute is unavailable the block must NOT promise a /work/uploads/ path that doesn't exist:\n\n{block}",
        );
    }

    #[test]
    fn format_block_with_python_available_promises_work_uploads_path() {
        let rows = [row(Some("data.csv"), "text/csv", "sha1")];
        let block = format_attached_files_block(&rows, true, None)
            .expect("block must emit when attachments exist");
        assert!(block.contains("data.csv"));
        assert!(
            block.contains("/work/uploads/data.csv"),
            "with python.execute available, the agent should be told where to read the file:\n\n{block}",
        );
    }

    #[test]
    fn format_block_lists_only_non_image_rows() {
        let rows = [
            row(Some("photo.png"), "image/png", "shapng"),
            row(Some("data.csv"), "text/csv", "shacsv"),
            row(Some("notes.md"), "text/markdown", "shamd"),
        ];
        let block =
            format_attached_files_block(&rows, true, None).expect("block should be present");
        assert!(block.contains("data.csv"));
        assert!(block.contains("notes.md"));
        assert!(
            !block.contains("photo.png"),
            "image rows should NOT appear in the prose block (they ride vision content):\n\n{block}",
        );
    }

    #[test]
    fn format_block_mentions_python_uploads_path_for_each_file() {
        let rows = [row(Some("data.csv"), "text/csv", "sha1")];
        let block = format_attached_files_block(&rows, true, None).unwrap();
        assert!(
            block.contains("/work/uploads/data.csv"),
            "block must tell the agent the exact path the file appears at:\n\n{block}",
        );
        assert!(
            block.contains("`python.execute`"),
            "block must namespace-attribute the tool so the model knows what to call",
        );
    }

    #[test]
    fn format_block_falls_back_to_hydration_compatible_name_when_filename_null() {
        // Critical contract: when state_attachments.filename is null
        // (legacy / transport-bridge rows), the prose block must
        // tell the agent the SAME filename hydration writes to disk.
        // Otherwise the agent's open() calls would 404.
        let rows = [row(None, "text/csv", "abcd1234ffffffff")];
        let block = format_attached_files_block(&rows, true, None).unwrap();
        // Hydration uses `derive_default_filename(mime, sha)` →
        // `attachment-<8 hex>.<ext>`. Block must match.
        let expected_name =
            crate::python_sandbox::service::derive_default_filename("text/csv", "abcd1234ffffffff");
        assert_eq!(expected_name, "attachment-abcd1234.csv");
        assert!(
            block.contains(&expected_name),
            "block must use the same fallback name as hydration; got:\n\n{block}",
        );
        assert!(
            block.contains(&format!("/work/uploads/{expected_name}")),
            "block must reference the path the file ACTUALLY ends up at",
        );
    }

    #[test]
    fn format_block_blank_filename_falls_back_to_derived() {
        // An empty-string filename (vs null) should also trip the
        // fallback — the operator-facing chip path empty-checks too.
        let rows = [row(Some(""), "application/json", "deadbeefcafebabe")];
        let block = format_attached_files_block(&rows, true, None).unwrap();
        assert!(block.contains("attachment-deadbeef.json"));
    }

    // ---- filename length cap (audit pass) ----

    #[test]
    fn sanitize_caps_filename_at_255_bytes() {
        let too_long = "a".repeat(300);
        let out = sanitize_filename(&too_long);
        assert!(
            out.len() <= 255,
            "sanitize must cap filename to 255 bytes; got {} bytes",
            out.len()
        );
    }

    #[test]
    fn sanitize_preserves_extension_when_truncating() {
        // The mime-by-extension routing in pandas needs the extension
        // intact. Truncate the stem, not the suffix.
        let raw = format!("{}.csv", "x".repeat(300));
        let out = sanitize_filename(&raw);
        assert!(out.ends_with(".csv"), "extension lost: {out}");
        assert!(out.len() <= 255);
    }

    #[test]
    fn sanitize_handles_multibyte_truncation_safely() {
        // Multibyte UTF-8 in the middle of the byte cap must not
        // panic (slicing on a non-char-boundary would).
        let raw = "🎵".repeat(100); // 400 bytes, each char is 4
        let out = sanitize_filename(&raw);
        assert!(out.len() <= 255);
        // Validity: must still be valid UTF-8 (the type system
        // guarantees this — but the test asserts non-empty after
        // truncation as a regression hedge).
        assert!(!out.is_empty());
    }

    #[test]
    fn sanitize_huge_extension_gives_up_gracefully() {
        // If the "extension" is itself huge, fall through to a
        // hard truncate rather than emit a name where the stem
        // is empty + the ext is the entire content.
        let raw = format!("a.{}", "z".repeat(300));
        let out = sanitize_filename(&raw);
        assert!(out.len() <= 255);
    }

    // ---- cross-module contract test ----

    #[test]
    fn format_block_dedupes_same_filename_to_one_line() {
        // The operator uploads data.csv twice in the same
        // conversation (different content). Hydration writes
        // /work/uploads/data.csv with the second blob — the first
        // is silently overwritten. The prose block must NOT list
        // both rows, because the agent's open("data.csv") will
        // only ever see one file. Two listings would confuse the
        // model (it'd hunt for "data (1).csv" or "data_2.csv" and
        // hit FileNotFoundError).
        let r1 = row(Some("data.csv"), "text/csv", "sha-1");
        let r2 = row(Some("data.csv"), "text/csv", "sha-2");
        let block = format_attached_files_block(&[r1, r2], true, None).unwrap();
        // Each kept row emits one bullet line. That line mentions
        // the filename twice (the backtick-wrapped name + the
        // `/work/uploads/<name>` path). So one bullet = two
        // substring hits of `data.csv`. Dedupe means: NOT four hits.
        let occurrences = block.matches("data.csv").count();
        assert_eq!(
            occurrences, 2,
            "after dedupe by filename, data.csv must appear in exactly one bullet \
             (= 2 substring hits: name + path). Block:\n\n{block}",
        );
        // Sanity: also assert only one bullet line.
        let bullets = block.lines().filter(|l| l.starts_with("* ")).count();
        assert_eq!(bullets, 1, "dedupe should leave one bullet, got {bullets}");
    }

    #[test]
    fn format_block_orders_files_alphabetically_for_kv_cache_stability() {
        // Same set of attachments must produce the same prose
        // across turns so the KV-cache prefix doesn't invalidate
        // for cosmetic reasons. BTreeMap iteration gives stable
        // alphabetical order; lock the behavior here.
        let rows = [
            row(Some("zebra.csv"), "text/csv", "sha-z"),
            row(Some("alpha.csv"), "text/csv", "sha-a"),
            row(Some("middle.csv"), "text/csv", "sha-m"),
        ];
        let block = format_attached_files_block(&rows, true, None).unwrap();
        let alpha_pos = block.find("alpha.csv").unwrap();
        let middle_pos = block.find("middle.csv").unwrap();
        let zebra_pos = block.find("zebra.csv").unwrap();
        assert!(
            alpha_pos < middle_pos && middle_pos < zebra_pos,
            "files should be alphabetical:\n\n{block}",
        );
    }

    // ------------------------------------------------------------
    // Filename collision suffix-on-conflict tests (2026-05-18,
    // remaining-work pass). Cover the cross-payload and
    // in-payload collision cases via the pure `unique_filename`
    // helper. The DB-touching `commit_decoded_attachments` is
    // exercised by integration tests; here we lock the algorithm.
    // ------------------------------------------------------------

    #[test]
    fn unique_filename_returns_base_when_no_collision() {
        let used = std::collections::HashSet::new();
        assert_eq!(unique_filename("data.csv", &used), "data.csv");
    }

    #[test]
    fn unique_filename_appends_paren_one_on_first_collision() {
        let used: std::collections::HashSet<String> = ["data.csv".to_owned()].into_iter().collect();
        assert_eq!(unique_filename("data.csv", &used), "data (1).csv");
    }

    #[test]
    fn unique_filename_increments_through_existing_suffixes() {
        let used: std::collections::HashSet<String> = [
            "data.csv".to_owned(),
            "data (1).csv".to_owned(),
            "data (2).csv".to_owned(),
        ]
        .into_iter()
        .collect();
        assert_eq!(unique_filename("data.csv", &used), "data (3).csv");
    }

    #[test]
    fn unique_filename_preserves_extension_for_pandas_routing() {
        // pandas + open() rely on the extension. The suffix must
        // go BEFORE the .csv, not after.
        let used: std::collections::HashSet<String> =
            ["report.pdf".to_owned()].into_iter().collect();
        let result = unique_filename("report.pdf", &used);
        assert!(result.ends_with(".pdf"), "extension lost: {result}");
        assert_eq!(result, "report (1).pdf");
    }

    #[test]
    fn unique_filename_handles_no_extension() {
        // Files without a clear extension — counter goes at the
        // very end.
        let used: std::collections::HashSet<String> = ["notes".to_owned()].into_iter().collect();
        assert_eq!(unique_filename("notes", &used), "notes (1)");
    }

    #[test]
    fn unique_filename_treats_huge_suffix_as_no_extension() {
        // If the "extension" is itself >16 bytes, treat as opaque
        // and counter at the end. Mirrors sanitize_filename's rule.
        let base = "weird.thisisaverylongextension";
        let used: std::collections::HashSet<String> = [base.to_owned()].into_iter().collect();
        let result = unique_filename(base, &used);
        // Counter went at the end, not before the long suffix.
        assert_eq!(result, format!("{base} (1)"));
    }

    #[test]
    fn unique_filename_bails_out_after_1000_collisions() {
        // Pathological case: someone seeded the conversation with
        // every numbered variant. Returns the base name and lets
        // the prose-dedupe + hydration handle it.
        let mut used: std::collections::HashSet<String> =
            ["data.csv".to_owned()].into_iter().collect();
        for n in 1..=1000 {
            used.insert(format!("data ({n}).csv"));
        }
        // After 1000 attempts the function returns base unchanged.
        assert_eq!(unique_filename("data.csv", &used), "data.csv");
    }

    #[test]
    fn unique_filename_50_collisions_stays_under_budget() {
        // Per-call budget: O(N) walk. 50 existing collisions is a
        // very heavy conversation (rare but plausible after a year
        // of "data.csv" uploads). Must complete fast enough that
        // the per-turn cost doesn't grow noticeably.
        let used: std::collections::HashSet<String> = (0..=50)
            .map(|n| {
                if n == 0 {
                    "data.csv".to_owned()
                } else {
                    format!("data ({n}).csv")
                }
            })
            .collect();
        let started = std::time::Instant::now();
        let result = unique_filename("data.csv", &used);
        let elapsed = started.elapsed();
        assert_eq!(result, "data (51).csv");
        assert!(
            elapsed < std::time::Duration::from_millis(10),
            "50-collision resolve took {elapsed:?} (budget: 10ms)",
        );
    }

    #[test]
    fn format_block_50_attachments_stays_under_budget() {
        // Per-turn budget guard (axiom #14 spirit, not Criterion-grade
        // since this isn't a true hot path). Format-block runs once
        // per agent turn. 50 attachments is a comfortably-high
        // operator-side ceiling; anything beyond that and the model's
        // context budget is the real bottleneck. 50 ms is enough
        // headroom for CI noise + Windows tempdir overhead but tight
        // enough to catch a regression that accidentally introduces
        // a network call or quadratic loop.
        let rows: Vec<_> = (0..50)
            .map(|i| {
                row(
                    Some(&format!("file-{i:02}.csv")),
                    "text/csv",
                    &format!("sha-{i:064}"),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let block = format_attached_files_block(&rows, true, None).unwrap();
        let elapsed = started.elapsed();
        assert!(
            block.lines().filter(|l| l.starts_with("* ")).count() == 50,
            "all 50 rows should produce bullets",
        );
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "50-row format block took {elapsed:?} (budget: 50ms)",
        );
    }

    // ------------------------------------------------------------
    // Vision-content encoder filter (2026-05-18 hotfix).
    //
    // Live bug: a CSV upload 500'd the agent turn because the
    // encoder was packaging the CSV bytes as a `data:text/csv;
    // base64,...` URL and shoving it into the OpenAI vision array.
    // The inference backend choked with "Failed to load image:
    // cannot identify image file <_io.BytesIO object>".
    //
    // Fix: only image-MIME rows feed the vision array. Non-image
    // attachments stay on disk and reach the agent via the
    // `build_attached_files_block` prose context + python.execute.
    // ------------------------------------------------------------

    // These tests use an in-memory DB. The encoder also reads
    // blob bytes from disk — we stub the path with a real
    // tempfile so the "read failed" warn doesn't fire.

    use execlaw_core::attachments::{AttachmentRow as CoreAttachmentRow, AttachmentStore};
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::ids::{AttachmentId as CoreAttachmentId, ConversationId as CoreConvoId};
    use execlaw_core::migrations::MigrationRunner;

    fn open_test_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn insert_row_with_bytes(
        db: &Database,
        cid: &CoreConvoId,
        mime: &str,
        bytes: &[u8],
        filename: Option<&str>,
    ) -> (String, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();
        let id = CoreAttachmentId::new();
        AttachmentStore::new(db)
            .insert(&CoreAttachmentRow {
                id: id.clone(),
                conversation_id: cid.clone(),
                mime_type: mime.to_owned(),
                path: tmp.path().to_string_lossy().into_owned(),
                sha256: format!("sha-test-{}", id.as_str()),
                received_at: 0,
                filename: filename.map(|s| s.to_owned()),
            })
            .unwrap();
        (id.as_str().to_owned(), tmp)
    }

    #[test]
    fn encoder_includes_image_attachments() {
        let db = open_test_db();
        let cid = CoreConvoId::new();
        let (id, _keep) =
            insert_row_with_bytes(&db, &cid, "image/png", &[0x89, 0x50, 0x4e, 0x47], None);
        let urls = encode_attachments_as_data_urls(&db, &cid, &[id]);
        assert_eq!(urls.len(), 1, "image must reach the vision array");
        assert!(urls[0].starts_with("data:image/png;base64,"));
    }

    #[test]
    fn encoder_skips_csv_attachments() {
        // The bug: previously this returned a `data:text/csv;
        // base64,...` URL which the inference backend rejected as
        // a malformed image.
        let db = open_test_db();
        let cid = CoreConvoId::new();
        let (id, _keep) =
            insert_row_with_bytes(&db, &cid, "text/csv", b"a,b\n1,2\n", Some("data.csv"));
        let urls = encode_attachments_as_data_urls(&db, &cid, &[id]);
        assert!(
            urls.is_empty(),
            "CSV must NOT enter the vision array — agent learns about it via the \
             attached-files prose block instead. urls={urls:?}",
        );
    }

    #[test]
    fn encoder_skips_every_non_image_mime() {
        let db = open_test_db();
        let cid = CoreConvoId::new();
        let mut ids = Vec::new();
        let mut keep_alive = Vec::new();
        for mime in [
            "text/csv",
            "text/tab-separated-values",
            "application/json",
            "text/plain",
            "text/markdown",
            "application/pdf",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.ms-excel",
        ] {
            let (id, keep) = insert_row_with_bytes(&db, &cid, mime, b"...", Some("file"));
            ids.push(id);
            keep_alive.push(keep);
        }
        let urls = encode_attachments_as_data_urls(&db, &cid, &ids);
        assert!(
            urls.is_empty(),
            "all data MIMEs must be excluded from vision; got {} url(s)",
            urls.len()
        );
    }

    #[test]
    fn encoder_includes_images_and_excludes_files_in_mixed_payload() {
        let db = open_test_db();
        let cid = CoreConvoId::new();
        let (img_id, _keep1) =
            insert_row_with_bytes(&db, &cid, "image/jpeg", &[0xff, 0xd8, 0xff], None);
        let (csv_id, _keep2) =
            insert_row_with_bytes(&db, &cid, "text/csv", b"x,y\n", Some("data.csv"));
        let urls = encode_attachments_as_data_urls(&db, &cid, &[img_id, csv_id]);
        assert_eq!(urls.len(), 1, "exactly the image must survive the filter");
        assert!(urls[0].starts_with("data:image/jpeg;base64,"));
    }

    // ------------------------------------------------------------
    // MessageAttachmentView projection — filename + size_bytes
    // surface so the SPA can render the file chip correctly.
    // ------------------------------------------------------------

    #[test]
    fn hydrate_message_attachments_passes_filename_through() {
        let db = open_test_db();
        let cid = CoreConvoId::new();
        let (id, _keep) =
            insert_row_with_bytes(&db, &cid, "text/csv", b"a,b\n1,2\n", Some("report.csv"));
        let views = hydrate_message_attachments(&db, &cid, &[id]);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].mime, "text/csv");
        assert_eq!(views[0].filename.as_deref(), Some("report.csv"));
        assert!(
            views[0].size_bytes > 0,
            "size should be the blob's on-disk size (got {})",
            views[0].size_bytes
        );
    }

    #[test]
    fn hydrate_message_attachments_falls_back_to_derived_name() {
        // Legacy / transport rows with no `filename` column should
        // still get a usable name from the same `derive_default_filename`
        // helper hydration uses on disk. Otherwise the SPA chip
        // would show "attachment" generically and a click would
        // download with a meaningless name.
        let db = open_test_db();
        let cid = CoreConvoId::new();
        let (id, _keep) = insert_row_with_bytes(&db, &cid, "text/csv", b"a\n", None);
        let views = hydrate_message_attachments(&db, &cid, &[id]);
        assert_eq!(views.len(), 1);
        assert!(
            views[0]
                .filename
                .as_deref()
                .map(|f| f.starts_with("attachment-") && f.ends_with(".csv"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn fallback_name_matches_hydration_helper_for_every_v1_mime() {
        // Lock the contract: the prose block's fallback name and
        // the hydration writer's filename are produced by the SAME
        // function call. If one diverges, the agent's open() path
        // breaks silently. This test would catch a refactor that
        // copy-pasted the logic into two places that drift.
        let sha = "1234567890abcdef".to_owned();
        for mime in [
            "text/csv",
            "text/tab-separated-values",
            "application/json",
            "text/plain",
            "text/markdown",
            "application/pdf",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.ms-excel",
        ] {
            let derived = crate::python_sandbox::service::derive_default_filename(mime, &sha);
            // Build a row with NULL filename + this mime; block
            // must reference exactly `derived`.
            let rows = [row(None, mime, &sha)];
            let block = format_attached_files_block(&rows, true, None).unwrap();
            assert!(
                block.contains(&derived),
                "mime {mime}: block missing derived name {derived}",
            );
        }
    }

    #[test]
    fn declared_mime_must_match_data_url_mime() {
        // Operator says "csv" but data URL says "exe" — reject.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"...");
        let r = InlineAttachmentRequest {
            mime: "text/csv".to_owned(),
            data_url: format!("data:application/x-msdownload;base64,{b64}"),
            filename: Some("data.csv".to_owned()),
        };
        let err = decode_inline_attachments(&[r]).expect_err("mime mismatch should reject");
        assert_eq!(err.code, "attachment_data_url_invalid");
    }
}
