//! Small utilities for the chats module.
//!
//! Grab-bag of helpers that don't fit cleanly into types / prompt /
//! attachments but are too small to deserve their own modules. The
//! through-line is "called from many places in `chats.rs`, no rich
//! domain knowledge of their own":
//!
//!   * [`event_log`] — DB + HMAC-key wrapper.
//!   * [`BusPhaseObserver`] + [`IdlePhaseGuard`] — phase-event
//!     plumbing the runner-tier turn paths thread into the event
//!     bus.
//!   * [`ensure_conversation`] / [`ensure_conversation_for`] /
//!     [`refresh_conversation_kind`] / [`apply_auto_display_name`]
//!     — conversation-row upserts the inbound + send paths share.
//!   * [`err_500`] — HTTP 500 builder with a tracing line attached.
//!   * [`rewrite_url_for_container`] / [`rewrite_url_with_alias`]
//!     — host loopback → host-gateway-alias rewrite for runner
//!     container reachability.
//!   * [`sanitize_generated_title`] / [`strip_think_blocks`] —
//!     post-process the title-generation model's output.
//!   * [`resolve_skill_prepend`] — operator-picked skill bodies →
//!     `<skill>` prepend block.

use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use execlaw_core::conversation::{
    ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
};
use execlaw_core::events::EventLog;
use execlaw_core::ids::{ConversationId, EventSeq};

use crate::chats::types::MAX_PREPEND_SKILL_BYTES;
use crate::events::UiEvent;
use crate::state::AppState;

/// Build an `EventLog` with the server's HMAC key attached (when set).
pub(crate) fn event_log(state: &AppState) -> EventLog<'_> {
    let log = EventLog::new(&state.db);
    match &state.event_log_hmac_key {
        Some(k) => log.with_hmac_key((**k).clone()),
        None => log,
    }
}

/// Phase 11.A — bridge from the runner's `PhaseObserver` trait to the
/// server's WS event bus. Construct one per turn with the active
/// `conversation_id`; every callback publishes a
/// `ConversationPhaseChanged` event that downstream subscribers
/// (SPA tabs, transport plugins) translate into typing-indicator
/// transitions.
pub(crate) struct BusPhaseObserver {
    pub(crate) events: crate::events::EventBus,
    pub(crate) conversation_id: String,
}

impl execlaw_runner_local::turn::PhaseObserver for BusPhaseObserver {
    fn observe(&self, phase: Phase) {
        self.events.publish(UiEvent::ConversationPhaseChanged {
            conversation_id: self.conversation_id.clone(),
            phase: phase.as_str().to_owned(),
        });
    }
}

/// RAII guard that publishes `phase=idle` on Drop unless explicitly
/// disarmed first. Closes the Phase 11 audit gap where every
/// `err_500` early-return left the typing indicator stuck on
/// "thinking" forever — every failure path now drops the guard,
/// which fires Idle on the way out.
///
/// Success paths call `disarm_after_publishing_idle()` to take
/// ownership of the publish (so the explicit Idle event still fires
/// before `ChatMessageOutbound`, matching the human "typing dots
/// stop a beat before the message lands" UX). After disarming, the
/// Drop is a no-op so we don't double-publish.
pub(crate) struct IdlePhaseGuard {
    events: crate::events::EventBus,
    conversation_id: String,
    armed: bool,
}

impl IdlePhaseGuard {
    pub(crate) fn new(events: crate::events::EventBus, conversation_id: String) -> Self {
        Self {
            events,
            conversation_id,
            armed: true,
        }
    }

    /// Publish Idle now and disable the Drop publish. Use on the
    /// success path so the Idle beat fires *before* the outbound
    /// reply event.
    pub(crate) fn disarm_after_publishing_idle(mut self) {
        self.events.publish(UiEvent::ConversationPhaseChanged {
            conversation_id: self.conversation_id.clone(),
            phase: Phase::Idle.as_str().to_owned(),
        });
        self.armed = false;
    }
}

impl Drop for IdlePhaseGuard {
    fn drop(&mut self) {
        if self.armed {
            self.events.publish(UiEvent::ConversationPhaseChanged {
                conversation_id: self.conversation_id.clone(),
                phase: Phase::Idle.as_str().to_owned(),
            });
        }
    }
}

/// Phase 4 — public wrapper over the file-private `ensure_conversation`
/// helper. The Signal inbound consumer needs to make sure the
/// `state_conversations` row exists before it binds the conversation
/// to a principal_group; exposing the helper avoids duplicating the
/// default-row construction.
pub fn ensure_conversation_for(db: &execlaw_core::Database, cid: &ConversationId) {
    let store = ConversationStore::new(db);
    ensure_conversation(&store, cid);
}

/// Apply a transport-supplied display_name to the conversation row.
/// Used by every transport inbound (Signal `groupName`, Signal DM
/// `source_name`, future WhatsApp / email / etc.) so the SPA's
/// sidebar mirrors whatever the source-of-truth system calls the
/// thread.
///
/// Tracking semantics (migration 0034):
///   * If the row's `display_name_source = 'manual'` (operator
///     renamed via `PATCH /api/chats/{id}`), leave it alone — the
///     operator's intent locks the value.
///   * If the row's source is `'auto'` (or unset, i.e. fresh row),
///     write the new name and tag it `'auto'`. Re-runs of the
///     same name are no-ops at the SQL layer (the `UPDATE ... WHERE
///     display_name <> ?` guard).
///
/// This is the path that picks up Signal group renames: signal-cli
/// sends `groupName` on every inbound, and an unchanged source
/// means a re-name from the original landed in our table the next
/// time someone posts.
///
/// Best-effort: errors log at debug and the routing flow continues.
/// Display name is a UX nicety, not correctness — failing the
/// inbound over a sidebar label would be the wrong trade-off.
pub fn apply_auto_display_name(
    db: &execlaw_core::Database,
    cid: &ConversationId,
    name: Option<&str>,
) {
    let trimmed = match name {
        Some(s) => s.trim(),
        None => return,
    };
    if trimmed.is_empty() {
        return;
    }
    let store = ConversationStore::new(db);
    match store.apply_auto_display_name(cid, trimmed) {
        Ok(_changed) => {}
        Err(e) => {
            tracing::debug!(
                target: "chats::apply_auto_display_name",
                conversation_id = %cid.as_str(),
                error = %e,
                "failed to apply transport-supplied display_name; sidebar will keep the old label",
            );
        }
    }
}

/// Trim and clean a model-generated title so the sidebar shows
/// something presentable. Strips wrapping quotes/backticks, trailing
/// punctuation, and `<think>` blocks the model might leak. Caps at
/// 60 chars defensively — the `<span>` ellipsis-truncates anyway,
/// but a 200-char "title" would blow the SPA's tooltip.
pub(crate) fn sanitize_generated_title(raw: &str) -> String {
    // Drop any think blocks the chat-template knob didn't catch.
    let stripped = strip_think_blocks(raw);
    let mut s = stripped.trim().to_owned();
    // Some models prefix with "Title:" despite the system prompt.
    for prefix in ["Title:", "title:", "TITLE:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim().to_owned();
        }
    }
    // Strip wrapping quotes/backticks (single or paired).
    let trims: &[char] = &['"', '\'', '`', '*', '#'];
    s = s.trim_matches(trims).to_owned();
    // Take just the first non-empty line — models occasionally
    // append a follow-up sentence.
    if let Some(first_line) = s.lines().find(|l| !l.trim().is_empty()) {
        s = first_line.trim().to_owned();
    }
    // Trailing period/comma/semicolon — strip.
    s = s.trim_end_matches(['.', ',', ';', ':']).to_owned();
    if s.chars().count() > 60 {
        s = s.chars().take(60).collect::<String>().trim().to_owned();
    }
    s
}

pub(crate) fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        if let Some(open) = lower.find("<think>") {
            out.push_str(&rest[..open]);
            if let Some(close_rel) = lower[open..].find("</think>") {
                let close = open + close_rel + "</think>".len();
                rest = &rest[close..];
            } else {
                // Unterminated — drop the rest.
                break;
            }
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}

/// Rewrite a URL so a Docker container can reach a service that
/// the host is running on its loopback. `127.0.0.1` and `localhost`
/// inside a container point at the container itself; the host is
/// reachable via `host.docker.internal` (Docker Desktop) or via
/// the `host-gateway` alias on Linux Docker (the bollard launcher
/// adds `--add-host host.docker.internal:host-gateway` for us).
///
/// Only rewrites the host portion of `http://localhost:...` and
/// `http://127.0.0.1:...`. Other hosts (real DNS names, container-
/// network names, IPs in non-loopback ranges) pass through
/// untouched — those already resolve correctly inside the runner.
///
/// Operators can override entirely via the `EXECLAW_RUNNER_HOST_ALIAS`
/// env var if their network setup uses a different name.
pub(crate) fn rewrite_url_for_container(url: &str) -> String {
    let alias = std::env::var("EXECLAW_RUNNER_HOST_ALIAS")
        .unwrap_or_else(|_| "host.docker.internal".to_owned());
    rewrite_url_with_alias(url, &alias)
}

/// Pure helper, alias supplied explicitly. Drives both the
/// production caller (`rewrite_url_for_container`) and the unit
/// tests so we don't have to mutate process env (which Rust
/// 2024 marks unsafe).
pub(crate) fn rewrite_url_with_alias(url: &str, alias: &str) -> String {
    // Cheap string scan: replace `://127.0.0.1` and `://localhost`
    // with `://<alias>` only when they appear immediately after the
    // scheme separator. Avoids accidentally munging path segments
    // that happen to contain "localhost".
    let lower = url.to_ascii_lowercase();
    if let Some(idx) = lower.find("://127.0.0.1") {
        let prefix = &url[..idx + 3];
        let suffix = &url[idx + 3 + "127.0.0.1".len()..];
        return format!("{prefix}{alias}{suffix}");
    }
    if let Some(idx) = lower.find("://localhost") {
        let prefix = &url[..idx + 3];
        let suffix = &url[idx + 3 + "localhost".len()..];
        return format!("{prefix}{alias}{suffix}");
    }
    url.to_owned()
}

pub(crate) fn ensure_conversation(store: &ConversationStore<'_>, cid: &ConversationId) {
    if matches!(store.get(cid), Ok(Some(_))) {
        return;
    }
    let row = ConversationRow {
        conversation_id: cid.clone(),
        kind: ConversationKind::ControllerDM,
        last_seq: EventSeq(0),
        phase: Phase::Idle,
        controller_id: None,
        trust_class: "Controller".into(),
        snapshot_blob: None,
        snapshot_seq: None,
        lease_owner: None,
        lease_expires: None,
        modality: Modality::Text,
        display_name: None,
        display_name_source: "auto".into(),
        is_pinned: false,
        is_ephemeral: false,
        ephemeral_expires_at: None,
        // 2026-04-28 — stamp so a freshly-minted conversation lands
        // at the TOP of the sidebar even before its first turn
        // commits. The chat handler bumps this again after the turn
        // completes; the first send overwrites this with whatever
        // wall-clock the turn finishes at.
        last_activity_at: chrono::Utc::now().timestamp(),
    };
    let _ = store.upsert(&row);
}

/// Re-derive the conversation kind + trust class after an inbound
/// message lands. Walks the existing row + the new sender's class
/// tag and persists the result. Single-participant for web chat
/// today; group conversations land with Phase 8 transports.
pub(crate) fn refresh_conversation_kind(
    store: &ConversationStore<'_>,
    cid: &ConversationId,
    sender_trust_tag: &str,
) {
    if let Ok(Some(mut row)) = store.get(cid) {
        let kind = ConversationKind::derive(&[sender_trust_tag]);
        if row.kind != kind {
            row.kind = kind;
        }
        // Track the most-restrictive trust class on the conversation
        // row — UI uses this to render the policy badge.
        row.trust_class = sender_trust_tag.to_owned();
        let _ = store.upsert(&row);
    }
}

pub(crate) fn err_500(msg: &str) -> axum::response::Response {
    tracing::error!("{msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

/// Resolve operator-picked skill names to a single prepended block
/// that gets prefixed onto the user text before the model sees it.
/// Returns the prepended block (empty `String` when no skills were
/// picked) — caller concatenates it with the original text. Each
/// skill renders as:
///
/// ```text
/// <skill name="foo/bar">
/// {body_md}
/// </skill>
///
/// ```
///
/// XML-style tags because the model parses them cleanly and the SPA
/// can regex-strip the same shape from the prepended `text` when
/// rendering the original user message in the bubble.
///
/// Errors:
///   * Unknown / archived skill → `(StatusCode::NOT_FOUND, "skill_not_found")`.
///   * Sum of resolved body bytes exceeds [`MAX_PREPEND_SKILL_BYTES`]
///     → `(StatusCode::PAYLOAD_TOO_LARGE, "skill_prepend_too_large")`.
pub(crate) fn resolve_skill_prepend(
    db: &execlaw_core::Database,
    names: &[String],
) -> Result<String, (StatusCode, &'static str, String)> {
    if names.is_empty() {
        return Ok(String::new());
    }
    let store = execlaw_skills::SkillStore::new(db.clone());
    let mut blocks = String::new();
    let mut total_bytes: usize = 0;
    for name in names {
        let view = store.view(name).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "skill_lookup_failed",
                format!("skill '{name}' lookup failed: {e}"),
            )
        })?;
        let Some(view) = view else {
            return Err((
                StatusCode::NOT_FOUND,
                "skill_not_found",
                format!("no skill named '{name}' (or it is archived)"),
            ));
        };
        total_bytes = total_bytes.saturating_add(view.body_md.len());
        if total_bytes > MAX_PREPEND_SKILL_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "skill_prepend_too_large",
                format!(
                    "skill bodies exceed prepend cap of {} bytes",
                    MAX_PREPEND_SKILL_BYTES
                ),
            ));
        }
        blocks.push_str(&format!(
            "<skill name=\"{}\">\n{}\n</skill>\n\n",
            name, view.body_md,
        ));
    }
    Ok(blocks)
}
