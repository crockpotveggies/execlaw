//! Handler dispatch — given a resolved origin + a prepared reply,
//! actually call the transport.
//!
//! Built-in handlers (core/web_socket_session, core/chat_append,
//! core/alert, core/drop) live here; plugin handlers route via
//! `plugin_host.call_tool("<plugin_id>.send_reply", { … })`.
//!
//! Slice 4 ships the dispatch skeleton with `drop` + `alert` fully
//! wired; `chat_append` + `web_socket_session` ship as TODO stubs
//! that succeed silently (slices 6/7 wire them to the real streams).
//! Plugin-channel dispatch IS wired since it's purely a call_tool
//! pass-through.

use super::capabilities::Capabilities;
use super::degrade::PreparedPart;
use super::tiers::PreparedReply;
use crate::state::AppState;
use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use execlaw_core::event_envelope::OriginRef;
use execlaw_core::ids::AlertId;
use execlaw_core::reply::ReplyPayload;

#[derive(Debug)]
pub enum SendError {
    /// Transient — caller may try the next tier in the fallback
    /// ladder. Examples: transport API rate-limited, transient
    /// network glitch, attachment too large for THIS tier (next
    /// tier strips attachments).
    Transient(String),
    /// Permanent — no fallback tier will help (e.g., origin handle
    /// has expired). Router fast-paths straight to the
    /// `on_failure` fallback.
    Permanent(String),
}

/// Dispatch one prepared reply to its handler.
pub async fn send(
    ctx: &super::RouterCtx,
    origin: &OriginRef,
    caps: &Capabilities,
    prepared: &PreparedReply,
) -> Result<String, SendError> {
    if caps.is_core() {
        // Built-in handler — dispatch on origin variant.
        match origin {
            OriginRef::WebSocketSession { session_id } => {
                send_web_socket(ctx, session_id, prepared).await
            }
            OriginRef::ChatAppend { conversation_id } => {
                send_chat_append(ctx, conversation_id, prepared).await
            }
            OriginRef::Alert => send_alert(ctx, prepared),
            OriginRef::None => Ok(String::new()),
            OriginRef::PluginChannel { .. } => Err(SendError::Permanent(format!(
                "origin claims plugin_channel but resolved handler is core/{}",
                caps.name
            ))),
        }
    } else {
        // Plugin-owned handler — call its registered send_reply tool.
        send_via_plugin(ctx, origin, caps, prepared).await
    }
}

/// TODO(slice 6) — stream the prepared deltas via `UiEvent::*` to the
/// SPA. For now we mark the reply as accepted so the tier ladder
/// completes; slice 6 wires the actual SSE push.
async fn send_web_socket(
    _ctx: &super::RouterCtx,
    _session_id: &str,
    _prepared: &PreparedReply,
) -> Result<String, SendError> {
    Ok(String::new())
}

/// TODO(slice 7) — append the prepared reply as a system-authored
/// message into the chat. The append must:
///   1. Mint an `EventRecord` of `kind = "system_msg"` (a new
///      `EventKind` variant — TODO).
///   2. Stamp `sender_principal_id = "system"`.
///   3. Carry the prepared parts as inline attachments / cards.
/// For now we return Ok so the fallback ladder doesn't hang.
async fn send_chat_append(
    _ctx: &super::RouterCtx,
    _conversation_id: &str,
    _prepared: &PreparedReply,
) -> Result<String, SendError> {
    Ok(String::new())
}

/// Fire an alert from the prepared text. Real implementation — used
/// by both `OriginRef::Alert` deliveries AND the fallback path's
/// "delivery failed" notification.
fn send_alert(ctx: &super::RouterCtx, prepared: &PreparedReply) -> Result<String, SendError> {
    let id = AlertId::new();
    let now = chrono::Utc::now().timestamp_millis();
    let title = first_line(&prepared.text).chars().take(120).collect::<String>();
    let detail = if prepared.text.len() > 120 {
        Some(prepared.text.clone())
    } else {
        None
    };
    let row = AlertRow {
        id: id.clone(),
        fingerprint: format!("reply_router::alert::{title}"),
        severity: Severity::Info,
        source: "reply_router".into(),
        title,
        detail,
        context_json: None,
        status: AlertStatus::Firing,
        first_seen_at: now,
        last_seen_at: now,
        occurrence_count: 1,
        resolved_at: None,
        resolved_by: None,
        ack_at: None,
        ack_by: None,
        snooze_until: None,
        incident_id: None,
        actions_json: None,
    };
    AlertStore::new(&ctx.db)
        .insert_firing(&row)
        .map(|_| id.as_str().to_owned())
        .map_err(|e| SendError::Transient(format!("alert insert failed: {e}")))
}

/// Route through the plugin's send_reply tool. Argument shape:
/// `{ "text": <string>, "parts": <ReplyPart[]>, "origin": <OriginRef> }`.
/// Plugins decode this in their Rhai handler.
async fn send_via_plugin(
    ctx: &super::RouterCtx,
    origin: &OriginRef,
    caps: &Capabilities,
    prepared: &PreparedReply,
) -> Result<String, SendError> {
    let tool_name = format!("{}.send_reply", caps.plugin_id);
    let args = serde_json::json!({
        "text": prepared.text,
        "parts": prepared.parts.iter().map(prepared_part_to_json).collect::<Vec<_>>(),
        "origin": origin,
    });
    match match &ctx.plugin_host {
        Some(h) => h,
        None => {
            return Err(SendError::Permanent(
                "no plugin_host wired into RouterCtx".into(),
            ));
        }
    }
        .call_tool(&tool_name, args, &["*"], Some("Controller"))
        .await
    {
        Ok(v) => Ok(v
            .get("message_id")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_owned()),
        Err(e) => {
            // Classify common permanent-failure strings; default to
            // Transient so the next tier gets a chance.
            if e.contains("not registered") || e.contains("not yet installed") {
                Err(SendError::Permanent(e))
            } else {
                Err(SendError::Transient(e))
            }
        }
    }
}

/// Last-resort: push the prepared payload into the operator's Inbox
/// thread. Used by `FailureFallback::ChatAppendHome`.
pub async fn deliver_to_operator_home(
    ctx: &super::RouterCtx,
    _payload: &ReplyPayload,
    primary_err: &str,
) -> Result<String, String> {
    // Look up (or mint) the controller's Inbox thread. In a multi-
    // operator setup we'd resolve from the run's actor principal;
    // for slice 4 we use the singleton controller.
    let controller_id = resolve_controller_principal_id(ctx)?;
    let _inbox = execlaw_core::operator_home::ensure_operator_home(&ctx.db, &controller_id)
        .map_err(|e| format!("ensure_operator_home: {e}"))?;
    // TODO(slice 7) — actually append a system-authored event to the
    // Inbox with the prepared payload + banner "Failed to deliver:
    // <primary_err>". For slice 4 we just confirm the Inbox exists
    // (so its row is in the DB) and return Ok so the fallback
    // resolution path completes.
    tracing::warn!(
        primary_err,
        "reply_router fallback: Inbox-append wiring is a TODO (slice 7)"
    );
    Ok(String::new())
}

/// Fire an alert summarizing a delivery failure. Used by every
/// fallback path so the operator notices something went wrong even
/// when the work product was successfully diverted to Inbox.
pub fn fire_delivery_failure_alert(
    ctx: &super::RouterCtx,
    payload: &ReplyPayload,
    primary_err: &str,
) -> Result<String, String> {
    let id = AlertId::new();
    let now = chrono::Utc::now().timestamp_millis();
    let title = format!("Reply delivery failed: {}", first_line(primary_err));
    let detail = Some(format!(
        "Original payload text (first 200 chars):\n{}\n\nError: {primary_err}",
        payload.text.chars().take(200).collect::<String>()
    ));
    let row = AlertRow {
        id: id.clone(),
        fingerprint: format!("reply_router::failure::{}", first_line(primary_err)),
        severity: Severity::Warning,
        source: "reply_router".into(),
        title,
        detail,
        context_json: None,
        status: AlertStatus::Firing,
        first_seen_at: now,
        last_seen_at: now,
        occurrence_count: 1,
        resolved_at: None,
        resolved_by: None,
        ack_at: None,
        ack_by: None,
        snooze_until: None,
        incident_id: None,
        actions_json: None,
    };
    AlertStore::new(&ctx.db)
        .insert_firing(&row)
        .map(|_| id.as_str().to_owned())
        .map_err(|e| format!("alert insert failed: {e}"))
}

/// Single-controller installs: resolve the Controller principal id
/// from `state_principals`. For multi-operator installs (future) this
/// becomes a per-run lookup based on the envelope's principal.
fn resolve_controller_principal_id(ctx: &super::RouterCtx) -> Result<String, String> {
    use rusqlite::OptionalExtension;
    ctx
        .db
        .with_conn(|c| {
            let id: Option<String> = c
                .query_row(
                    "SELECT id FROM state_principals \
                     WHERE trust_class = 'Controller' \
                     ORDER BY first_seen ASC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(id)
        })
        .map_err(|e| format!("controller lookup: {e}"))?
        .ok_or_else(|| "no Controller principal present".to_owned())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_owned()
}

fn prepared_part_to_json(p: &PreparedPart) -> serde_json::Value {
    match p {
        PreparedPart::TextLine(text) => serde_json::json!({"kind": "text_line", "text": text}),
        PreparedPart::Attachment {
            kind,
            id,
            url,
            filename,
            mime_type,
            caption,
            size_bytes,
        } => serde_json::json!({
            "kind": "attachment",
            "ref_kind": match kind {
                super::degrade::AttachmentRefKind::Attachment => "attachment",
                super::degrade::AttachmentRefKind::Artifact => "artifact",
            },
            "id": id,
            "url": url,
            "filename": filename,
            "mime_type": mime_type,
            "caption": caption,
            "size_bytes": size_bytes,
        }),
        PreparedPart::Card { title, fields } => serde_json::json!({
            "kind": "card",
            "title": title,
            "fields": fields,
        }),
        PreparedPart::InlineChart {
            spec,
            theme,
            caption,
        } => serde_json::json!({
            "kind": "inline_chart",
            "spec": spec,
            "theme": theme.as_str(),
            "caption": caption,
        }),
        PreparedPart::InlineTable {
            columns,
            rows,
            caption,
        } => serde_json::json!({
            "kind": "inline_table",
            "columns": columns,
            "rows": rows,
            "caption": caption,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_strips_subsequent_lines() {
        assert_eq!(first_line("a\nb\nc"), "a");
        assert_eq!(first_line("solo"), "solo");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn prepared_part_text_line_serializes_to_text_line_kind() {
        let p = PreparedPart::TextLine("hello".into());
        let v = prepared_part_to_json(&p);
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("text_line"));
        assert_eq!(v.get("text").and_then(|x| x.as_str()), Some("hello"));
    }
}
