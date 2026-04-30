//! DB-backed implementations of the `tool::*Api` capability traits.
//!
//! Each impl wraps the existing `Store` types and enforces caller-
//! trust + caller-conversation scoping internally. The tool authors
//! never see a `Database` handle — they hold an `Arc<dyn FooApi>`
//! and call its narrow methods.
//!
//! Production wiring: the dispatch layer constructs the impls per
//! call (cheap — they're thin wrappers over `Database` clones, which
//! are themselves `Arc`-based) and stuffs the right ones into the
//! `ToolCtx` based on what the tool's descriptor declared.
//!
//! 2026-04-29.

use crate::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use crate::conversation::ConversationStore;
use crate::db::Database;
use crate::ids::{AlertId, ConversationId};
use crate::memory::{MemoryEntry, MemoryStore};
use crate::routines::{
    next_fire_after, parse_cron, parse_timezone, RoutineRow, RoutineStore, RoutineUpsert,
};
use crate::tool::{
    ApiError, ConversationApi, HistoryEntry, MemoryApi, MemoryListEntry, NotifyApi,
    NotifyReceipt, NotifySeverity, RoutineSummary, ScheduleApi, ThreadInfo,
};
use async_trait::async_trait;
use rusqlite::params;
use serde::Deserialize;

// -----------------------------------------------------------------
// Trust ranking — local to this module so `core` stays free of
// `execlaw-policy`. The vocabulary intentionally matches `policy`'s
// `TrustLevel` so strings round-trip cleanly.
// -----------------------------------------------------------------

const TRUST_CLASSES_HIGH_TO_LOW: &[&str] = &[
    "Controller",
    "Delegated",
    "KnownTrusted",
    "KnownLimited",
    "UnknownPending",
    "Blocked",
];

fn trust_rank(class: &str) -> Option<u8> {
    TRUST_CLASSES_HIGH_TO_LOW
        .iter()
        .position(|&c| c == class)
        // Index 0 is the highest, but we want highest = highest rank.
        // Subtract from len-1 so Controller=5, Blocked=0.
        .map(|i| (TRUST_CLASSES_HIGH_TO_LOW.len() - 1 - i) as u8)
}

/// Whether a caller at `caller` is allowed to read memory tagged
/// `target`. Read-up is forbidden; read-at-or-below is allowed.
fn can_read(caller: &str, target: &str) -> bool {
    match (trust_rank(caller), trust_rank(target)) {
        (Some(a), Some(b)) => a >= b,
        // Unknown class strings are treated as the lowest possible
        // — never allowed to read anything labeled with a known
        // class. This is a conservative choice: a typo'd trust class
        // string at the dispatch layer fails closed.
        _ => false,
    }
}

/// Compute the chain of trust classes a caller can read, highest
/// first. Used by `MemoryApi::read` to cascade through the trust
/// classes the caller can see.
fn readable_classes(caller: &str) -> Vec<&'static str> {
    TRUST_CLASSES_HIGH_TO_LOW
        .iter()
        .copied()
        .filter(|c| can_read(caller, c))
        .collect()
}

// -----------------------------------------------------------------
// ConversationApi: DB-backed
// -----------------------------------------------------------------

/// Tightest reasonable cap on the thread display name — three short
/// English words rarely exceed 30 chars; we allow 64 for proper
/// nouns / multi-word names. Counted in chars (not bytes) so emoji
/// titles don't false-trip the cap.
pub const MAX_THREAD_DISPLAY_NAME_LEN: usize = 64;

/// Hard cap on the number of history rows a single `read_history`
/// call can return. Larger windows are more often a sign of the
/// LLM trying to dump the whole transcript than a real need; the
/// dispatcher trims `limit` to this value.
pub const MAX_HISTORY_LIMIT: u32 = 200;

// Internal payload mirrors of the server-side message structs so
// `tool_apis` can pull `text` out without depending on `crates/server`.
// MessagePack-encoded by the event log; we decode on read here.
#[derive(Debug, Deserialize)]
struct UserMsgTextPayload {
    text: String,
}

#[derive(Debug, Deserialize)]
struct ModelTurnTextPayload {
    text: String,
}

/// DB-backed `ConversationApi`. Captures the caller's
/// `conversation_id` at construction so the trait methods can never
/// reach a different conversation.
pub struct DbConversationApi {
    db: Database,
    conversation_id: ConversationId,
}

impl DbConversationApi {
    pub fn new(db: Database, conversation_id: ConversationId) -> Self {
        Self {
            db,
            conversation_id,
        }
    }
}

#[async_trait]
impl ConversationApi for DbConversationApi {
    async fn get_thread(&self) -> Result<ThreadInfo, ApiError> {
        let db = self.db.clone();
        let cid = self.conversation_id.clone();
        let cid_for_err = cid.clone();
        let row = tokio::task::spawn_blocking(move || ConversationStore::new(&db).get(&cid))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(|e| ApiError::Storage(format!("conversation get: {e}")))?
            .ok_or_else(|| {
                ApiError::NotFound(format!("conversation {}", cid_for_err.as_str()))
            })?;
        Ok(ThreadInfo {
            conversation_id: row.conversation_id.as_str().to_owned(),
            display_name: row.display_name,
        })
    }

    async fn set_thread_name(&self, raw: &str) -> Result<(), ApiError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ApiError::Validation(
                "thread name is empty after trimming".into(),
            ));
        }
        let chars = trimmed.chars().count();
        if chars > MAX_THREAD_DISPLAY_NAME_LEN {
            return Err(ApiError::Validation(format!(
                "thread name too long ({chars} chars; max {MAX_THREAD_DISPLAY_NAME_LEN})"
            )));
        }
        let db = self.db.clone();
        let cid = self.conversation_id.clone();
        let name = trimmed.to_owned();
        tokio::task::spawn_blocking(move || {
            ConversationStore::new(&db).set_display_name(&cid, Some(&name))
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("set_display_name: {e}")))?;
        Ok(())
    }

    async fn read_history(
        &self,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, ApiError> {
        let limit = limit.min(MAX_HISTORY_LIMIT).max(1) as i64;
        // i64::MAX as the "no upper bound" sentinel — every real seq
        // is < this, so the predicate becomes a tautology and the
        // ORDER BY DESC LIMIT clause runs as expected.
        let before = before_seq.unwrap_or(i64::MAX);
        let db = self.db.clone();
        let cid = self.conversation_id.clone();

        let rows: Vec<(i64, String, Vec<u8>, i64)> = tokio::task::spawn_blocking(move || {
            db.with_conn(|c| {
                let mut stmt = c.prepare_cached(
                    "SELECT seq, kind, payload, committed_at \
                     FROM state_events \
                     WHERE conversation_id = ?1 \
                       AND seq < ?2 \
                       AND kind IN ('user_msg', 'model_turn') \
                     ORDER BY seq DESC \
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(params![cid.as_str(), before, limit], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("read_history: {e}")))?;

        let mut out = Vec::with_capacity(rows.len());
        for (seq, kind, payload, committed_at) in rows {
            let (role, text) = match kind.as_str() {
                "user_msg" => {
                    let p: UserMsgTextPayload = rmp_serde::from_slice(&payload)
                        .unwrap_or(UserMsgTextPayload { text: String::new() });
                    ("user".to_string(), p.text)
                }
                "model_turn" => {
                    let p: ModelTurnTextPayload = rmp_serde::from_slice(&payload)
                        .unwrap_or(ModelTurnTextPayload { text: String::new() });
                    ("agent".to_string(), p.text)
                }
                other => (other.to_string(), String::new()),
            };
            out.push(HistoryEntry {
                seq,
                role,
                text,
                committed_at,
            });
        }
        Ok(out)
    }
}

// -----------------------------------------------------------------
// MemoryApi: DB-backed
// -----------------------------------------------------------------

/// DB-backed `MemoryApi`. Captures the caller's `caller_trust` at
/// construction so reads cascade through the right set of trust
/// classes and writes always land at the caller's level.
pub struct DbMemoryApi {
    db: Database,
    caller_trust: String,
    clock_now_unix: i64,
}

impl DbMemoryApi {
    pub fn new(db: Database, caller_trust: impl Into<String>, now_unix: i64) -> Self {
        Self {
            db,
            caller_trust: caller_trust.into(),
            clock_now_unix: now_unix,
        }
    }
}

#[async_trait]
impl MemoryApi for DbMemoryApi {
    async fn read(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<String>, ApiError> {
        let db = self.db.clone();
        let scope = scope.to_owned();
        let key = key.to_owned();
        let classes: Vec<&'static str> = readable_classes(&self.caller_trust);
        if classes.is_empty() {
            // Trust class string didn't parse to anything we know —
            // fail closed. Any unknown caller can't read anyone's memory.
            return Err(ApiError::NotAuthorized(format!(
                "trust class {:?} cannot read memory",
                self.caller_trust
            )));
        }
        let got = tokio::task::spawn_blocking(move || {
            let store = MemoryStore::new(&db);
            for class in classes {
                let entry = store.get(&scope, class, &key)?;
                if let Some(entry) = entry {
                    return Ok::<_, crate::DbError>(Some(entry));
                }
            }
            Ok(None)
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("memory read: {e}")))?;

        match got {
            None => Ok(None),
            Some(entry) => {
                let s = String::from_utf8(entry.value_blob).map_err(|_| {
                    ApiError::Storage(
                        "stored memory value is not valid utf-8".into(),
                    )
                })?;
                Ok(Some(s))
            }
        }
    }

    async fn write(
        &self,
        scope: &str,
        key: &str,
        value: &str,
    ) -> Result<(), ApiError> {
        if trust_rank(&self.caller_trust).is_none() {
            return Err(ApiError::NotAuthorized(format!(
                "trust class {:?} cannot write memory",
                self.caller_trust
            )));
        }
        let entry = MemoryEntry {
            scope: scope.to_owned(),
            trust_class: self.caller_trust.clone(),
            key: key.to_owned(),
            value_blob: value.as_bytes().to_vec(),
            ttl_expires: None,
            updated_at: self.clock_now_unix,
        };
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || MemoryStore::new(&db).upsert(&entry))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(|e| ApiError::Storage(format!("memory write: {e}")))?;
        Ok(())
    }

    async fn list(
        &self,
        scope: &str,
        prefix: &str,
    ) -> Result<Vec<MemoryListEntry>, ApiError> {
        // The underlying `MemoryStore` doesn't have a scan method yet
        // (Phase 1 stub). Returning an empty list with the same shape
        // keeps the contract honest until that lands.
        let _ = (scope, prefix);
        Ok(Vec::new())
    }
}

// -----------------------------------------------------------------
// NotifyApi: wraps `AlertStore` so a tool can reach the operator
// through the existing alerts/dropdown infrastructure.
// -----------------------------------------------------------------

/// Source label every agent-fired notification carries so the
/// Settings → Alerts page can distinguish them from system alerts.
const AGENT_NOTIFY_SOURCE: &str = "tool.notify_controller";

fn severity_from_notify(s: NotifySeverity) -> Severity {
    match s {
        NotifySeverity::Info => Severity::Info,
        NotifySeverity::Warning => Severity::Warning,
        NotifySeverity::Error => Severity::Error,
        NotifySeverity::Critical => Severity::Critical,
    }
}

/// SHA-256 → hex of `(conversation_id || severity || title || detail)`
/// so duplicate notifications dedup against the same firing alert
/// (per `AlertStore::insert_firing`'s fingerprint-based dedup path).
/// We hash the title/detail rather than embed it raw so very long
/// messages don't bloat the fingerprint string.
fn build_notify_fingerprint(
    cid: &str,
    severity: NotifySeverity,
    title: &str,
    detail: Option<&str>,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cid.hash(&mut h);
    severity.as_str().hash(&mut h);
    title.hash(&mut h);
    detail.unwrap_or("").hash(&mut h);
    format!("{}:notify:{:016x}", cid, h.finish())
}

/// DB-backed `NotifyApi`. Captures the caller's `conversation_id`
/// at construction so `notify` can stamp the right thread on the
/// alert row.
pub struct DbNotifyApi {
    db: Database,
    conversation_id: ConversationId,
    clock_now_unix: i64,
}

impl DbNotifyApi {
    pub fn new(db: Database, conversation_id: ConversationId, now_unix: i64) -> Self {
        Self {
            db,
            conversation_id,
            clock_now_unix: now_unix,
        }
    }
}

#[async_trait]
impl NotifyApi for DbNotifyApi {
    async fn notify(
        &self,
        severity: NotifySeverity,
        title: &str,
        detail: Option<&str>,
    ) -> Result<NotifyReceipt, ApiError> {
        let trimmed_title = title.trim();
        if trimmed_title.is_empty() {
            return Err(ApiError::Validation(
                "notification title is empty after trimming".into(),
            ));
        }
        if trimmed_title.chars().count() > 200 {
            return Err(ApiError::Validation(format!(
                "notification title too long ({} chars; max 200)",
                trimmed_title.chars().count()
            )));
        }
        if let Some(d) = detail
            && d.chars().count() > 4_000
        {
            return Err(ApiError::Validation(format!(
                "notification detail too long ({} chars; max 4000)",
                d.chars().count()
            )));
        }

        let fingerprint = build_notify_fingerprint(
            self.conversation_id.as_str(),
            severity,
            trimmed_title,
            detail,
        );
        let now = self.clock_now_unix;
        let db = self.db.clone();
        let title = trimmed_title.to_owned();
        let detail = detail.map(|s| s.to_owned());

        let (row_to_insert, fingerprint_clone) = (
            AlertRow {
                id: AlertId::new(),
                fingerprint: fingerprint.clone(),
                severity: severity_from_notify(severity),
                source: AGENT_NOTIFY_SOURCE.to_owned(),
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
            },
            fingerprint,
        );
        let row_id = row_to_insert.id.clone();

        let dedup_check = tokio::task::spawn_blocking(move || {
            let store = AlertStore::new(&db);
            // Probe for existing firing alert at this fingerprint so
            // we can report `deduplicated` accurately. Both branches
            // converge on `insert_firing` doing the right thing
            // (insert vs occurrence-bump).
            let existed_before = store.firing_id_for_fingerprint(&fingerprint_clone)?;
            store.insert_firing(&row_to_insert)?;
            Ok::<_, crate::DbError>(existed_before.is_some())
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("notify_controller: {e}")))?;

        Ok(NotifyReceipt {
            alert_id: row_id.as_str().to_owned(),
            deduplicated: dedup_check,
        })
    }
}

// -----------------------------------------------------------------
// ScheduleApi: wraps `RoutineStore` so the agent can manage recurring
// tasks through the same store the operator's Settings UI uses.
// -----------------------------------------------------------------

fn row_to_summary(row: &RoutineRow) -> RoutineSummary {
    RoutineSummary {
        id: row.id.clone(),
        name: row.name.clone(),
        schedule_cron: row.schedule_cron.clone(),
        timezone: row.timezone.clone(),
        prompt: row.prompt.clone(),
        target_conversation_id: row.target_conversation_id.clone(),
        enabled: row.enabled,
        last_run_at: row.last_run_at,
        last_run_status: row.last_run_status.map(|s| s.as_str().to_owned()),
        next_run_at: row.next_run_at,
    }
}

fn map_routine_err(e: crate::routines::RoutineError) -> ApiError {
    use crate::routines::RoutineError;
    match e {
        RoutineError::Invalid(s) => ApiError::Validation(s),
        RoutineError::NotFound(s) => ApiError::NotFound(s),
        RoutineError::Db(e) => ApiError::Storage(e.to_string()),
        RoutineError::Sqlite(e) => ApiError::Storage(e.to_string()),
    }
}

/// DB-backed `ScheduleApi`. Captures the caller's `caller_trust` +
/// `conversation_id` at construction so the implementation can reject
/// privileged operations from low-trust callers (e.g. scheduling a
/// routine to fire into a different conversation).
pub struct DbScheduleApi {
    db: Database,
    caller_trust: String,
    caller_conversation_id: ConversationId,
    clock_now_unix: i64,
}

impl DbScheduleApi {
    pub fn new(
        db: Database,
        caller_trust: impl Into<String>,
        caller_conversation_id: ConversationId,
        now_unix: i64,
    ) -> Self {
        Self {
            db,
            caller_trust: caller_trust.into(),
            caller_conversation_id,
            clock_now_unix: now_unix,
        }
    }

    /// Whether the caller is allowed to target a different
    /// conversation than their own. Controllers can; everyone else
    /// gets clamped to their own thread.
    fn can_target_other_conversation(&self) -> bool {
        self.caller_trust == "Controller"
    }
}

#[async_trait]
impl ScheduleApi for DbScheduleApi {
    async fn create_routine(
        &self,
        name: &str,
        schedule_cron: &str,
        prompt: &str,
        target_conversation_id: Option<&str>,
        timezone: Option<&str>,
    ) -> Result<RoutineSummary, ApiError> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err(ApiError::Validation("routine name is empty".into()));
        }
        if trimmed_name.chars().count() > 200 {
            return Err(ApiError::Validation(
                "routine name too long (max 200 chars)".into(),
            ));
        }
        if prompt.trim().is_empty() {
            return Err(ApiError::Validation("routine prompt is empty".into()));
        }
        if prompt.chars().count() > 8_000 {
            return Err(ApiError::Validation(
                "routine prompt too long (max 8000 chars)".into(),
            ));
        }
        // Validate cron + tz before hitting the DB so the error
        // surfaces with the right ApiError variant.
        let tz_str = timezone.unwrap_or("UTC");
        parse_cron(schedule_cron).map_err(map_routine_err)?;
        parse_timezone(tz_str).map_err(map_routine_err)?;

        // Trust scoping: only the controller can target another
        // conversation. Everyone else is forced to their own.
        let target = match target_conversation_id {
            Some(other)
                if other != self.caller_conversation_id.as_str()
                    && !self.can_target_other_conversation() =>
            {
                return Err(ApiError::NotAuthorized(format!(
                    "trust class {:?} cannot target conversation {other} for scheduling",
                    self.caller_trust
                )));
            }
            Some(other) => Some(other.to_owned()),
            None => Some(self.caller_conversation_id.as_str().to_owned()),
        };

        let upsert = RoutineUpsert {
            id: None,
            name: trimmed_name.to_owned(),
            schedule_cron: schedule_cron.to_owned(),
            timezone: tz_str.to_owned(),
            prompt: prompt.to_owned(),
            target_conversation_id: target,
            enabled: true,
        };
        let now = self.clock_now_unix;
        let db = self.db.clone();
        let row = tokio::task::spawn_blocking(move || {
            RoutineStore::new(&db).upsert(&upsert, now)
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(map_routine_err)?;
        Ok(row_to_summary(&row))
    }

    async fn list_routines(&self) -> Result<Vec<RoutineSummary>, ApiError> {
        let db = self.db.clone();
        let rows = tokio::task::spawn_blocking(move || RoutineStore::new(&db).list_all())
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(map_routine_err)?;
        Ok(rows.iter().map(row_to_summary).collect())
    }

    async fn get_routine(&self, id: &str) -> Result<Option<RoutineSummary>, ApiError> {
        let db = self.db.clone();
        let id = id.to_owned();
        let row = tokio::task::spawn_blocking(move || RoutineStore::new(&db).get(&id))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(map_routine_err)?;
        Ok(row.as_ref().map(row_to_summary))
    }

    async fn update_routine(
        &self,
        id: &str,
        name: Option<&str>,
        schedule_cron: Option<&str>,
        prompt: Option<&str>,
        target_conversation_id: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<RoutineSummary, ApiError> {
        let id_owned = id.to_owned();
        let db_for_get = self.db.clone();
        let existing = tokio::task::spawn_blocking(move || {
            RoutineStore::new(&db_for_get).get(&id_owned)
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(map_routine_err)?
        .ok_or_else(|| ApiError::NotFound(format!("routine {id}")))?;

        let new_name = name.unwrap_or(&existing.name).to_owned();
        if new_name.trim().is_empty() {
            return Err(ApiError::Validation("routine name is empty".into()));
        }
        let new_cron = schedule_cron.unwrap_or(&existing.schedule_cron).to_owned();
        let new_prompt = prompt.unwrap_or(&existing.prompt).to_owned();
        let new_target = match target_conversation_id {
            Some(s) if !self.can_target_other_conversation()
                && s != self.caller_conversation_id.as_str() =>
            {
                return Err(ApiError::NotAuthorized(format!(
                    "trust class {:?} cannot retarget routine to {s}",
                    self.caller_trust
                )));
            }
            Some(s) => Some(s.to_owned()),
            None => existing.target_conversation_id.clone(),
        };
        let new_enabled = enabled.unwrap_or(existing.enabled);
        parse_cron(&new_cron).map_err(map_routine_err)?;

        let upsert = RoutineUpsert {
            id: Some(existing.id.clone()),
            name: new_name,
            schedule_cron: new_cron,
            timezone: existing.timezone.clone(),
            prompt: new_prompt,
            target_conversation_id: new_target,
            enabled: new_enabled,
        };
        let now = self.clock_now_unix;
        let db = self.db.clone();
        let row = tokio::task::spawn_blocking(move || {
            RoutineStore::new(&db).upsert(&upsert, now)
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(map_routine_err)?;
        Ok(row_to_summary(&row))
    }

    async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<RoutineSummary, ApiError> {
        // Implemented as a constrained `update_routine` so the
        // single store path enforces the same validation.
        self.update_routine(id, None, None, None, None, Some(enabled)).await
    }

    async fn delete_routine(&self, id: &str) -> Result<bool, ApiError> {
        let db = self.db.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || RoutineStore::new(&db).delete(&id))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(map_routine_err)
    }
}

#[allow(dead_code)] // referenced by future scheduler-execution wiring
fn touch_used_helper(db: &Database) {
    let _ = next_fire_after;
    let _ = db;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{
        ConversationKind, ConversationRow, Modality, Phase,
    };
    use crate::db::DbConfig;
    use crate::ids::EventSeq;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_conversation(db: &Database, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(db)
            .upsert(&ConversationRow {
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
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    // --- Trust ranking ------------------------------------------------

    #[test]
    fn trust_ranks_match_intended_order() {
        assert!(trust_rank("Controller").unwrap() > trust_rank("Delegated").unwrap());
        assert!(trust_rank("Delegated").unwrap() > trust_rank("KnownTrusted").unwrap());
        assert!(trust_rank("KnownTrusted").unwrap() > trust_rank("KnownLimited").unwrap());
        assert!(trust_rank("KnownLimited").unwrap() > trust_rank("UnknownPending").unwrap());
        assert!(trust_rank("UnknownPending").unwrap() > trust_rank("Blocked").unwrap());
    }

    #[test]
    fn unknown_trust_class_does_not_rank() {
        assert!(trust_rank("Goblin").is_none());
        assert!(trust_rank("").is_none());
    }

    #[test]
    fn can_read_enforces_no_read_up() {
        assert!(can_read("Controller", "Delegated"));
        assert!(can_read("Controller", "Controller"));
        assert!(!can_read("KnownTrusted", "Controller"));
        assert!(!can_read("Blocked", "Controller"));
        // Unknown caller never reads.
        assert!(!can_read("Goblin", "Controller"));
    }

    #[test]
    fn readable_classes_chain_is_caller_then_below() {
        let chain = readable_classes("KnownTrusted");
        assert_eq!(chain.first(), Some(&"KnownTrusted"));
        assert_eq!(chain.last(), Some(&"Blocked"));
        assert!(!chain.contains(&"Controller"));
        assert!(!chain.contains(&"Delegated"));
    }

    // --- ConversationApi ----------------------------------------------

    #[tokio::test]
    async fn conversation_api_get_returns_thread_info() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        ConversationStore::new(&db)
            .set_display_name(&cid, Some("First Topic"))
            .unwrap();
        let api = DbConversationApi::new(db, cid);
        let info = api.get_thread().await.unwrap();
        assert_eq!(info.conversation_id, "c1");
        assert_eq!(info.display_name.as_deref(), Some("First Topic"));
    }

    #[tokio::test]
    async fn conversation_api_set_name_writes_through() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c2");
        let api = DbConversationApi::new(db.clone(), cid.clone());
        api.set_thread_name("Q4 budget").await.unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Q4 budget"));
    }

    #[tokio::test]
    async fn conversation_api_set_name_trims_and_rejects_empty() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c3");
        let api = DbConversationApi::new(db.clone(), cid.clone());

        api.set_thread_name("  Trimmed  ").await.unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Trimmed"));

        match api.set_thread_name("   ").await.unwrap_err() {
            ApiError::Validation(_) => {}
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn conversation_api_set_name_enforces_64_char_cap() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c4");
        let api = DbConversationApi::new(db, cid);

        let ok_64 = "a".repeat(MAX_THREAD_DISPLAY_NAME_LEN);
        api.set_thread_name(&ok_64).await.unwrap();

        let too_long = "a".repeat(MAX_THREAD_DISPLAY_NAME_LEN + 1);
        match api.set_thread_name(&too_long).await.unwrap_err() {
            ApiError::Validation(msg) => assert!(msg.contains("too long")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Multi-byte chars count as 1 each — emoji titles stay legal even
    /// though their byte length is large.
    #[tokio::test]
    async fn conversation_api_counts_chars_not_bytes() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c5");
        let api = DbConversationApi::new(db.clone(), cid.clone());
        api.set_thread_name("📌📋💬").await.unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("📌📋💬"));
    }

    #[tokio::test]
    async fn conversation_api_get_missing_conversation_is_not_found() {
        let db = fresh_db();
        let api = DbConversationApi::new(db, ConversationId::from("nope"));
        match api.get_thread().await.unwrap_err() {
            ApiError::NotFound(s) => assert!(s.contains("nope")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // --- read_history -------------------------------------------------

    use crate::events::{EventKind, EventLog, EventRecord};
    use serde::Serialize;

    #[derive(Debug, Serialize)]
    struct TextPayload {
        text: String,
    }

    fn append_text_event(
        log: &EventLog<'_>,
        cid: &ConversationId,
        seq: i64,
        kind: EventKind,
        text: &str,
    ) {
        let ev = EventRecord::new(
            cid.clone(),
            crate::ids::EventSeq(seq),
            kind,
            &TextPayload { text: text.into() },
            Some("controller".into()),
        )
        .unwrap();
        log.append(&ev).unwrap();
    }

    #[tokio::test]
    async fn read_history_returns_user_and_model_events_newest_first() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ch1");
        let log = EventLog::new(&db);
        append_text_event(&log, &cid, 1, EventKind::UserMsg, "hello");
        append_text_event(&log, &cid, 2, EventKind::ModelTurn, "hi back");
        append_text_event(&log, &cid, 3, EventKind::UserMsg, "another");

        let api = DbConversationApi::new(db, cid);
        let entries = api.read_history(None, 50).await.unwrap();
        assert_eq!(entries.len(), 3);
        // Newest first: seq 3, 2, 1.
        assert_eq!(entries[0].seq, 3);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].text, "another");
        assert_eq!(entries[1].seq, 2);
        assert_eq!(entries[1].role, "agent");
        assert_eq!(entries[1].text, "hi back");
        assert_eq!(entries[2].seq, 1);
        assert_eq!(entries[2].role, "user");
        assert_eq!(entries[2].text, "hello");
    }

    /// Internal/operational events (alerts, voice, phase markers) must
    /// never leak into the chat-history view — only `user_msg` and
    /// `model_turn` are currently surfaced.
    #[tokio::test]
    async fn read_history_filters_out_non_chat_event_kinds() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ch2");
        let log = EventLog::new(&db);
        append_text_event(&log, &cid, 1, EventKind::UserMsg, "hi");
        append_text_event(&log, &cid, 2, EventKind::AlertFired, "alert payload");
        append_text_event(&log, &cid, 3, EventKind::Wakeup, "wakeup");
        append_text_event(&log, &cid, 4, EventKind::ModelTurn, "reply");

        let api = DbConversationApi::new(db, cid);
        let entries = api.read_history(None, 50).await.unwrap();
        assert_eq!(entries.len(), 2);
        let seqs: Vec<i64> = entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![4, 1]);
    }

    /// Pagination via `before_seq`: passing `Some(N)` returns events
    /// with seq < N, newest-first within that window.
    #[tokio::test]
    async fn read_history_paginates_via_before_seq() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ch3");
        let log = EventLog::new(&db);
        for i in 1..=5 {
            append_text_event(&log, &cid, i, EventKind::UserMsg, &format!("msg {i}"));
        }
        let api = DbConversationApi::new(db, cid);

        // Page 1 — newest 2.
        let p1 = api.read_history(None, 2).await.unwrap();
        let seqs: Vec<i64> = p1.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![5, 4]);

        // Page 2 — next 2 before seq 4.
        let p2 = api.read_history(Some(4), 2).await.unwrap();
        let seqs: Vec<i64> = p2.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 2]);

        // Page 3 — last one.
        let p3 = api.read_history(Some(2), 2).await.unwrap();
        let seqs: Vec<i64> = p3.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1]);
    }

    /// Limit is hard-capped at MAX_HISTORY_LIMIT — a tool that
    /// requests 10_000 events gets at most MAX_HISTORY_LIMIT back.
    #[tokio::test]
    async fn read_history_clamps_limit_to_max() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ch4");
        let log = EventLog::new(&db);
        // Insert 10 events and ask for "10000" — should still cap.
        // 10 < cap, so we just verify the count doesn't somehow exceed it.
        for i in 1..=10 {
            append_text_event(&log, &cid, i, EventKind::UserMsg, "x");
        }
        let api = DbConversationApi::new(db, cid);
        let entries = api.read_history(None, 10_000).await.unwrap();
        assert!(entries.len() as u32 <= MAX_HISTORY_LIMIT);
        assert_eq!(entries.len(), 10);
    }

    /// Zero limit gets bumped to 1 (defensive — a `limit: 0` request
    /// from a buggy LLM otherwise returns nothing useful).
    #[tokio::test]
    async fn read_history_rejects_zero_limit_by_clamping_up() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ch5");
        let log = EventLog::new(&db);
        append_text_event(&log, &cid, 1, EventKind::UserMsg, "hi");
        let api = DbConversationApi::new(db, cid);
        let entries = api.read_history(None, 0).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    /// Adversarial: the tool calls read_history on its own
    /// `conversation_id`, so a malicious LLM can't probe other
    /// conversations by passing a different id (the impl captures
    /// `cid` at construction; method has no conversation arg).
    #[tokio::test]
    async fn read_history_only_returns_caller_conversation_events() {
        let db = fresh_db();
        let _other = seed_conversation(&db, "other-conv");
        let mine = seed_conversation(&db, "my-conv");
        let log = EventLog::new(&db);
        append_text_event(&log, &_other, 1, EventKind::UserMsg, "their secret");
        append_text_event(&log, &mine, 1, EventKind::UserMsg, "my hello");

        let api = DbConversationApi::new(db, mine);
        let entries = api.read_history(None, 50).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "my hello");
    }

    /// Decode failure on a malformed payload yields an empty `text`
    /// rather than failing the whole call — the LLM still sees the
    /// other events. This is defensive: we never want one corrupt
    /// row to take down the whole read_history call.
    #[tokio::test]
    async fn read_history_tolerates_corrupt_payload() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ch6");
        // Insert a row with a payload that won't decode as TextPayload.
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_events (conversation_id, seq, kind, payload, committed_at, actor, key_id) \
                 VALUES (?1, 1, 'user_msg', ?2, 1, 'controller', 0)",
                rusqlite::params![cid.as_str(), &b"not-msgpack"[..]],
            )?;
            Ok(())
        })
        .unwrap();
        let api = DbConversationApi::new(db, cid);
        let entries = api.read_history(None, 50).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "");
    }

    // --- MemoryApi ----------------------------------------------------

    #[tokio::test]
    async fn memory_api_write_then_read_at_same_class() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db, "Controller", 0);
        api.write("global", "k", "hello").await.unwrap();
        let v = api.read("global", "k").await.unwrap();
        assert_eq!(v.as_deref(), Some("hello"));
    }

    /// Adversarial: low-trust caller cannot read controller memory.
    #[tokio::test]
    async fn memory_api_low_trust_cannot_read_controller() {
        let db = fresh_db();
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("global", "secret", "top-secret")
            .await
            .unwrap();
        let outsider = DbMemoryApi::new(db, "UnknownPending", 0);
        let v = outsider.read("global", "secret").await.unwrap();
        assert_eq!(v, None);
    }

    /// Writes always land at caller's class — model can't escalate by
    /// pretending. The capability layer doesn't even let the LLM
    /// supply a trust_class field; a faulty future caller that did
    /// would still be ignored because `caller_trust` is captured at
    /// construction.
    #[tokio::test]
    async fn memory_api_write_always_at_caller_class() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db.clone(), "KnownLimited", 0);
        api.write("s", "k", "v").await.unwrap();
        let store = MemoryStore::new(&db);
        assert!(store.get("s", "KnownLimited", "k").unwrap().is_some());
        assert!(store.get("s", "Controller", "k").unwrap().is_none());
    }

    /// Cascading reads: a Controller can read memories at every level
    /// down through Blocked. Higher-precedence (Controller) wins on
    /// conflicting keys.
    #[tokio::test]
    async fn memory_api_cascade_read_picks_highest_class_first() {
        let db = fresh_db();
        DbMemoryApi::new(db.clone(), "KnownTrusted", 0)
            .write("s", "k", "from-known-trusted")
            .await
            .unwrap();
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("s", "k", "from-controller")
            .await
            .unwrap();
        let v = DbMemoryApi::new(db, "Controller", 0)
            .read("s", "k")
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some("from-controller"));
    }

    #[tokio::test]
    async fn memory_api_unknown_trust_class_fails_closed_on_read() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db, "Goblin", 0);
        match api.read("s", "k").await.unwrap_err() {
            ApiError::NotAuthorized(_) => {}
            other => panic!("expected NotAuthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_api_unknown_trust_class_fails_closed_on_write() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db, "Goblin", 0);
        match api.write("s", "k", "v").await.unwrap_err() {
            ApiError::NotAuthorized(_) => {}
            other => panic!("expected NotAuthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_api_list_is_currently_empty_stub() {
        let db = fresh_db();
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("s", "k1", "v")
            .await
            .unwrap();
        let v = DbMemoryApi::new(db, "Controller", 0)
            .list("s", "")
            .await
            .unwrap();
        assert!(v.is_empty());
    }
}
