//! Operational alerts for chat-turn failures (§10).
//!
//! The backend supervisor only watches *container liveness* — a vLLM
//! that boots, returns 200 on `/v1/models`, and 400s every chat
//! completion (e.g. missing `--enable-auto-tool-choice`) is invisible
//! to it. Without an additional signal, an operator only finds out
//! when they sit down to a chat that won't reply, dig through
//! `~/.execlaw/logs/`, and notice 500s.
//!
//! This module bridges the gap. Every failed chat turn fires an alert
//! whose fingerprint is derived from the failure's *root cause* — so
//! 100 failed user messages collapse to 1 row in the alerts panel
//! with `occurrence_count = 100`, instead of 100 separate alerts.
//! The next *successful* turn auto-resolves any open alerts under
//! this source so the operator's badge clears without manual ack.
//!
//! What counts as a chat failure here is intentionally narrow: any
//! `Err(_)` returned by `run_runner_turn` / `run_tool_capable_turn` /
//! `run_real_turn` / `run_stub_turn`. Cancellations and approvals
//! flow through the `Ok` path and don't trip this surface.

use execlaw_core::Database;
use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use execlaw_core::ids::AlertId;
use tracing::warn;

const ALERT_SOURCE: &str = "chats";
const FINGERPRINT_PREFIX: &str = "chats:turn_failure:";

/// Insert (or bump the occurrence count of) a Firing alert that
/// describes a chat-turn failure. Best-effort: a DB hiccup here must
/// NOT bubble back into the user-visible 500 path.
///
/// `root_cause` is a short label derived from the anyhow chain (see
/// `extract_root_cause`). `which_turn` distinguishes the four turn
/// kinds (`"runner"`, `"tool"`, `"real"`, `"stub"`) so the panel
/// row reads "runner turn failed" rather than a generic "turn
/// failed" — same source, different fingerprint suffix.
pub fn fire_turn_failure(
    db: &Database,
    which_turn: &str,
    root_cause: &str,
    conversation_id: &str,
) {
    let store = AlertStore::new(db);
    let now = chrono::Utc::now().timestamp();
    let fingerprint = format!(
        "{FINGERPRINT_PREFIX}{which_turn}:{}",
        normalize_for_fingerprint(root_cause),
    );
    let row = AlertRow {
        id: AlertId::new(),
        fingerprint,
        severity: Severity::Error,
        source: ALERT_SOURCE.to_owned(),
        title: format!("{which_turn} turn failed: {root_cause}"),
        detail: Some(format!(
            "Recent conversation: {conversation_id}\n\nRoot cause: {root_cause}"
        )),
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
    if let Err(e) = store.insert_firing(&row) {
        warn!(error = %e, "chat alert insert failed");
    }
}

/// Resolve every Firing alert under the chat-turn-failure source.
/// Called from the success path so that "chats are working again"
/// clears the panel without manual ack — same shape as
/// `backend_supervisor::resolve_crashloop_alert`.
pub fn resolve_turn_failure_alerts(db: &Database) {
    let store = AlertStore::new(db);
    let firing = match store.list(Some(&[AlertStatus::Firing]), Some(200)) {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "chat alert list failed");
            return;
        }
    };
    let now = chrono::Utc::now().timestamp();
    for row in firing.iter().filter(|a| a.fingerprint.starts_with(FINGERPRINT_PREFIX)) {
        let _ = store.resolve(&row.id, "chats", now);
    }
}

/// Pull the most-significant segment from an anyhow chain. With
/// `format!("{e:#}")` the chain is rendered as
/// `"top: middle: ...: root"` — the last colon-separated segment is
/// usually the actual upstream error (HTTP status + body, transport
/// failure, etc). Fall back to the whole string for anything that
/// doesn't match the pattern.
pub fn extract_root_cause(err_chain: &str) -> &str {
    err_chain.rsplit(": ").next().unwrap_or(err_chain).trim()
}

/// Cheap normaliser so similar-but-not-identical errors share a
/// fingerprint. Keeps the first 80 chars (long bodies blow up the
/// fingerprint), lowercases, and strips characters that would make
/// the fingerprint annoying to read in logs (newlines, quotes).
fn normalize_for_fingerprint(s: &str) -> String {
    let trimmed = s.trim();
    let truncated = if trimmed.len() > 80 {
        &trimmed[..80]
    } else {
        trimmed
    };
    truncated
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            '"' | '\'' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::ids::AlertId;
    use execlaw_core::{Database, DbConfig};

    fn db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        execlaw_core::MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn extract_root_cause_pulls_last_segment_of_anyhow_chain() {
        // The exact shape `format!("{e:#}")` produces for an
        // `anyhow::Error` chain.
        assert_eq!(
            extract_root_cause(
                "runner turn failed: opening inference stream: 400 Bad Request"
            ),
            "400 Bad Request",
        );
    }

    #[test]
    fn extract_root_cause_falls_back_to_whole_string_when_no_separator() {
        assert_eq!(extract_root_cause("oneliner"), "oneliner");
    }

    #[test]
    fn fire_then_fire_again_dedups_to_one_row_with_count_2() {
        let db = db();
        fire_turn_failure(&db, "runner", "400 Bad Request", "conv-1");
        fire_turn_failure(&db, "runner", "400 Bad Request", "conv-2");
        let store = AlertStore::new(&db);
        let firing = store.list(Some(&[AlertStatus::Firing]), Some(10)).unwrap();
        assert_eq!(firing.len(), 1, "two identical fires must dedup");
        assert_eq!(firing[0].occurrence_count, 2);
    }

    #[test]
    fn fire_with_different_root_causes_produces_separate_rows() {
        let db = db();
        fire_turn_failure(&db, "runner", "400 Bad Request", "conv-1");
        fire_turn_failure(&db, "runner", "connection refused", "conv-1");
        let store = AlertStore::new(&db);
        let firing = store.list(Some(&[AlertStatus::Firing]), Some(10)).unwrap();
        assert_eq!(
            firing.len(),
            2,
            "different root causes must NOT collapse: {firing:?}",
        );
    }

    #[test]
    fn different_turn_kinds_produce_separate_rows() {
        // A runner failure and a real-turn failure with the same
        // upstream message are still separate operator-facing
        // problems, so we don't collapse them.
        let db = db();
        fire_turn_failure(&db, "runner", "boom", "c1");
        fire_turn_failure(&db, "real", "boom", "c1");
        let store = AlertStore::new(&db);
        let firing = store.list(Some(&[AlertStatus::Firing]), Some(10)).unwrap();
        assert_eq!(firing.len(), 2);
    }

    #[test]
    fn resolve_clears_every_open_chat_alert_but_leaves_others_alone() {
        let db = db();
        fire_turn_failure(&db, "runner", "boom", "c1");
        fire_turn_failure(&db, "real", "kaboom", "c2");

        // Insert an unrelated alert under a different source — it
        // must NOT be resolved.
        let store = AlertStore::new(&db);
        let now = chrono::Utc::now().timestamp();
        store
            .insert_firing(&AlertRow {
                id: AlertId::new(),
                fingerprint: "backend_supervisor:Standard:img".into(),
                severity: Severity::Error,
                source: "backend_supervisor".into(),
                title: "unrelated".into(),
                detail: None,
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
            })
            .unwrap();

        resolve_turn_failure_alerts(&db);

        let firing = store.list(Some(&[AlertStatus::Firing]), Some(10)).unwrap();
        assert_eq!(firing.len(), 1, "unrelated alert must survive: {firing:?}");
        assert_eq!(firing[0].source, "backend_supervisor");
    }

    #[test]
    fn long_root_causes_get_truncated_in_fingerprint() {
        // Without truncation, two errors that share the first 80
        // chars would produce different fingerprints — so a
        // 4096-char vLLM body wouldn't dedup. Verify they DO
        // collapse here.
        let db = db();
        let long_a = format!("400 Bad Request: {}", "a".repeat(500));
        let long_b = format!("400 Bad Request: {}", "a".repeat(900));
        fire_turn_failure(&db, "runner", &long_a, "c1");
        fire_turn_failure(&db, "runner", &long_b, "c2");
        let store = AlertStore::new(&db);
        let firing = store.list(Some(&[AlertStatus::Firing]), Some(10)).unwrap();
        assert_eq!(firing.len(), 1);
        assert_eq!(firing[0].occurrence_count, 2);
    }
}
