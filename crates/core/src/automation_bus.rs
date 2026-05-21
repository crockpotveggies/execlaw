//! Durable substrate for the automation event bus (M1).
//!
//! Owns the `state_bus_events` table â€” DISTINCT from `state_events`
//! (the per-conversation event log). The two tables share no foreign
//! keys, no rows, and no invariants. They are independent substrates:
//!
//!   * `state_events` is conversation-scoped, append-only, HMAC-
//!     chained, and replayed to reconstruct turn state.
//!   * `state_bus_events` is the durable inbox for external signals
//!     (webhooks, socket transports, plugin emits, routine fires)
//!     that automations subscribe to.
//!
//! This module contains pure data + DB code:
//!
//!   * `Event` envelope shape
//!   * `BusEventKind` (separate enum from `crate::events::EventKind`)
//!   * `BusEventStore` (insert + read + retention primitives)
//!
//! The dispatcher (mpsc consumer + worker pool) and the internal
//! poller live in `crates/server/src/automation_bus.rs` because they
//! need tokio + `AppState`. Keeping persistence here mirrors how
//! `routines.rs` (core) and `routine_runner.rs` (server) are split.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// Kinds of events flowing through the automation bus.
///
/// Distinct from the per-conversation `crate::events::EventKind` enum.
/// Additive â€” new kinds land without breaking existing consumers
/// because `parse` falls back to `Other` for unknown strings.
///
/// Serde representation: each variant uses `#[serde(rename = ...)]`
/// to match the canonical wire form returned by `as_str()`. This
/// keeps the JSON-in-DB (e.g., `state_automations.definition`'s
/// `trigger.kind`) lined up with the kind column in
/// `state_bus_events`, so the matcher SQL
/// `json_extract(definition, '$.trigger.kind') = state_bus_events.kind`
/// works without a translation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum BusEventKind {
    /// A webhook arrived at `/api/webhooks/{plugin_id}/...`. Fired
    /// alongside the existing plugin Rhai handler dispatch â€” the bus
    /// emission does not change webhook handling, it just makes the
    /// raw receipt observable to automations.
    #[serde(rename = "webhook.received")]
    WebhookReceived,
    /// A message arrived over a socket-based transport (Signal,
    /// WhatsApp, Matrix, ...). Reserved here so transport plugins
    /// can opt in without a follow-up migration; ingress wiring is
    /// a later milestone.
    #[serde(rename = "socket.message")]
    SocketMessage,
    /// A plugin emitted a structured event. The plugin supplies the
    /// `source` (e.g., `plugin:weather`) and the payload shape;
    /// the bus stays opinion-free about contents.
    #[serde(rename = "plugin.emit")]
    PluginEmit,
    /// A routine completed (success, failure, or skipped). Lets
    /// automations react to routine outcomes without needing a
    /// separate cron trigger.
    #[serde(rename = "routine.fired")]
    RoutineFired,
    /// Escape hatch â€” only emitted by `parse` when an unknown kind
    /// string is read from disk. NOT a meaningful producer-side
    /// value: a producer that constructs `Event { kind: Other, .. }`
    /// writes the literal string `"other"` to the row, losing any
    /// semantic kind. Producers introducing new categories should
    /// add a variant to this enum + update `parse` / `as_str`.
    #[serde(rename = "other")]
    Other,
}

impl BusEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BusEventKind::WebhookReceived => "webhook.received",
            BusEventKind::SocketMessage => "socket.message",
            BusEventKind::PluginEmit => "plugin.emit",
            BusEventKind::RoutineFired => "routine.fired",
            BusEventKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "webhook.received" => BusEventKind::WebhookReceived,
            "socket.message" => BusEventKind::SocketMessage,
            "plugin.emit" => BusEventKind::PluginEmit,
            "routine.fired" => BusEventKind::RoutineFired,
            _ => BusEventKind::Other,
        }
    }
}

/// Producer-supplied event envelope. `id` is the dedup key (PK in
/// `state_bus_events`). Producers that want dedup semantics across
/// upstream retries supply a stable ID (content hash, upstream
/// message ID); producers that don't care supply a random ULID.
///
/// `envelope` (M6) carries the reply target, sender identity, and
/// correlation id. Producers from before the M6 migration may omit
/// it (`None`) â€” the matcher fills in `EventEnvelope::system_internal()`
/// before dispatch so flow authors always see a populated envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub kind: BusEventKind,
    pub source: String,
    pub received_at: i64,
    pub payload: serde_json::Value,
    /// Optional on the publish-side for backward compat (existing
    /// call sites in cmd_serve / plugin_webhook_routes don't yet
    /// build envelopes). Defaults to `system_internal()` at insert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<crate::event_envelope::EventEnvelope>,
}

/// Stored row, fetched by the dispatcher / poller / retention sweeper.
#[derive(Debug, Clone, PartialEq)]
pub struct BusEventRow {
    pub id: String,
    pub kind: BusEventKind,
    pub source: String,
    pub received_at: i64,
    pub payload: serde_json::Value,
    pub internal: bool,
    pub dispatched_at: Option<i64>,
    /// Always populated post-read â€” defaults to `system_internal()`
    /// for legacy rows that wrote NULL.
    pub envelope: crate::event_envelope::EventEnvelope,
}

#[derive(Debug, Error)]
pub enum BusEventError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("payload serialize: {0}")]
    Payload(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Row was newly inserted. The dispatcher should pick it up.
    Inserted,
    /// PK collision â€” a duplicate. Caller can treat as success but
    /// MUST NOT re-enqueue (the original row's dispatch is the
    /// authoritative one). First write wins; later writes are
    /// silently dropped.
    Duplicate,
}

/// DB-facing primitives. Owns no async + no channel â€” purely sync
/// SQLite. The server-side `AutomationBus` wraps this and layers
/// the `tokio::sync::mpsc` dispatch channel on top.
pub struct BusEventStore<'a> {
    db: &'a Database,
}

/// Decode `envelope_json` from a row, falling back to
/// `system_internal()` on null / parse failure so downstream code
/// always sees a populated envelope (M6 backward-compat shim â€” once
/// every producer is migrated, NULL rows should age out via the
/// `state_bus_events` retention sweeper).
fn decode_envelope(raw: Option<String>) -> crate::event_envelope::EventEnvelope {
    match raw {
        Some(s) => serde_json::from_str(&s)
            .unwrap_or_else(|_| crate::event_envelope::EventEnvelope::system_internal()),
        None => crate::event_envelope::EventEnvelope::system_internal(),
    }
}

impl<'a> BusEventStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Insert an event. `internal = false` means the row will ride
    /// the in-process mpsc channel; `true` means the SQLite poller
    /// will pick it up (used by in-process producers to avoid
    /// producer-consumer deadlock through the channel).
    pub fn publish(&self, evt: &Event, internal: bool) -> Result<PublishOutcome, BusEventError> {
        let payload = serde_json::to_string(&evt.payload)?;
        let kind = evt.kind.as_str();
        let internal_flag: i64 = if internal { 1 } else { 0 };
        // M6: persist envelope when present. Producers that haven't
        // been migrated yet write NULL; the read-side fills in
        // `system_internal()` so flow authors always see a populated
        // envelope.
        let envelope_json = evt
            .envelope
            .as_ref()
            .map(|e| serde_json::to_string(e).expect("envelope must serialize"));
        let inserted = self.db.with_conn(|c| {
            let n = c.execute(
                "INSERT INTO state_bus_events \
                 (id, kind, source, received_at, payload, internal, envelope_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO NOTHING",
                params![
                    &evt.id,
                    kind,
                    &evt.source,
                    evt.received_at,
                    &payload,
                    internal_flag,
                    envelope_json,
                ],
            )?;
            Ok(n)
        })?;
        Ok(if inserted > 0 {
            PublishOutcome::Inserted
        } else {
            PublishOutcome::Duplicate
        })
    }

    /// Atomically claim an event for dispatch. Returns `Ok(true)`
    /// when this caller is the first to mark the row, `Ok(false)`
    /// when another caller (live dispatcher vs. crash-recovery scan,
    /// poller vs. recovery, etc.) already claimed it.
    ///
    /// This is the **race guard** for at-least-once delivery: only
    /// the caller who gets `true` should invoke the handler. Without
    /// this guard, a crash-recovery scan racing a live mpsc delivery
    /// of the same row would fire the handler twice.
    pub fn mark_dispatched(&self, id: &str, dispatched_at: i64) -> Result<bool, BusEventError> {
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_bus_events SET dispatched_at = ?2 \
                 WHERE id = ?1 AND dispatched_at IS NULL",
                params![id, dispatched_at],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Fetch a single row by id. The dispatcher uses this after
    /// pulling an id off the mpsc â€” the channel carries only ids
    /// to keep the queue layer trivially cheap.
    ///
    /// Payload deserialization fallback: if the stored JSON is
    /// somehow malformed (only possible via DB corruption â€” the
    /// publish path always writes valid JSON via `serde_json::to_string`)
    /// the payload surfaces as `Null` and a warning is logged. We
    /// don't fail the read, because the dispatcher should keep
    /// draining other events rather than wedge on one bad row.
    pub fn get(&self, id: &str) -> Result<Option<BusEventRow>, BusEventError> {
        let row = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, received_at, payload, internal, dispatched_at, envelope_json \
                 FROM state_bus_events WHERE id = ?1",
            )?;
            let r = stmt
                .query_row([id], |r| {
                    let row_id: String = r.get(0)?;
                    let payload_str: String = r.get(4)?;
                    let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                event_id = %row_id,
                                error = %e,
                                "automation bus: stored payload is not valid JSON; surfacing as Null (DB corruption?)",
                            );
                            serde_json::Value::Null
                        }
                    };
                    let internal_flag: i64 = r.get(5)?;
                    let envelope = decode_envelope(r.get::<_, Option<String>>(7)?);
                    Ok(BusEventRow {
                        id: row_id,
                        kind: BusEventKind::parse(&r.get::<_, String>(1)?),
                        source: r.get(2)?,
                        received_at: r.get(3)?,
                        payload,
                        internal: internal_flag != 0,
                        dispatched_at: r.get(6)?,
                        envelope,
                    })
                })
                .ok();
            Ok(r)
        })?;
        Ok(row)
    }

    /// Return the last `limit` events of a given kind, newest first.
    /// Backs the editor's "sample payload" picker (M4c) â€” when the
    /// operator wants to test-run an automation, they pick from a
    /// dropdown of recently-observed events of the matching kind so
    /// they don't have to hand-craft the payload.
    pub fn list_recent_for_kind(
        &self,
        kind: BusEventKind,
        limit: i64,
    ) -> Result<Vec<BusEventRow>, BusEventError> {
        let kind_str = kind.as_str();
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, received_at, payload, internal, dispatched_at, envelope_json \
                 FROM state_bus_events \
                 WHERE kind = ?1 \
                 ORDER BY received_at DESC \
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![kind_str, limit], |r| {
                let row_id: String = r.get(0)?;
                let payload_str: String = r.get(4)?;
                let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
                    Ok(v) => v,
                    Err(_) => serde_json::Value::Null,
                };
                let internal_flag: i64 = r.get(5)?;
                let envelope = decode_envelope(r.get::<_, Option<String>>(7)?);
                Ok(BusEventRow {
                    id: row_id,
                    kind: BusEventKind::parse(&r.get::<_, String>(1)?),
                    source: r.get(2)?,
                    received_at: r.get(3)?,
                    payload,
                    internal: internal_flag != 0,
                    dispatched_at: r.get(6)?,
                    envelope,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    /// Return the IDs of rows the dispatcher still owes work for,
    /// oldest first. Workhorse for two paths:
    ///
    ///   * Crash recovery at process start (`internal_only = false`)
    ///   * The 100ms internal poller (`internal_only = true`)
    ///
    /// `limit` bounds memory; callers typically batch in chunks of
    /// 256-1024 and loop until the result is short of the limit.
    pub fn fetch_pending(
        &self,
        internal_only: bool,
        limit: i64,
    ) -> Result<Vec<String>, BusEventError> {
        let ids = self.db.with_conn(|c| {
            let sql = if internal_only {
                "SELECT id FROM state_bus_events \
                 WHERE dispatched_at IS NULL AND internal = 1 \
                 ORDER BY received_at ASC LIMIT ?1"
            } else {
                "SELECT id FROM state_bus_events \
                 WHERE dispatched_at IS NULL \
                 ORDER BY received_at ASC LIMIT ?1"
            };
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(ids)
    }

    /// Retention sweep: delete dispatched rows older than the cutoff.
    /// Pending rows are NEVER swept regardless of age â€” retention
    /// should not paper over a stuck dispatcher.
    pub fn purge_dispatched_older_than(&self, cutoff_unix: i64) -> Result<usize, BusEventError> {
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_bus_events \
                 WHERE dispatched_at IS NOT NULL \
                   AND received_at < ?1",
                params![cutoff_unix],
            )?;
            Ok(n)
        })?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn sample_event(id: &str, source: &str, ts: i64) -> Event {
        Event {
            id: id.into(),
            kind: BusEventKind::WebhookReceived,
            source: source.into(),
            received_at: ts,
            payload: serde_json::json!({"k": "v"}),
            envelope: None,
        }
    }

    #[test]
    fn publish_inserts_new_row() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        let e = sample_event("evt-1", "ring", 100);
        let out = store.publish(&e, false).unwrap();
        assert_eq!(out, PublishOutcome::Inserted);
        let row = store.get("evt-1").unwrap().unwrap();
        assert_eq!(row.kind, BusEventKind::WebhookReceived);
        assert_eq!(row.source, "ring");
        assert_eq!(row.received_at, 100);
        assert!(!row.internal);
        assert_eq!(row.dispatched_at, None);
    }

    #[test]
    fn publish_duplicate_id_returns_duplicate_outcome_first_write_wins() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        let first = sample_event("evt-1", "ring", 100);
        let second = Event {
            id: "evt-1".into(),
            kind: BusEventKind::PluginEmit,           // different
            source: "different".into(),               // different
            received_at: 999,                         // different
            payload: serde_json::json!({"k2": "v2"}), // different
            envelope: None,
        };
        assert_eq!(
            store.publish(&first, false).unwrap(),
            PublishOutcome::Inserted
        );
        assert_eq!(
            store.publish(&second, false).unwrap(),
            PublishOutcome::Duplicate
        );
        let row = store.get("evt-1").unwrap().unwrap();
        // PK conflict does NOT overwrite â€” operator intent is
        // preserved by the producer's choice of stable ID.
        assert_eq!(row.kind, BusEventKind::WebhookReceived);
        assert_eq!(row.source, "ring");
        assert_eq!(row.received_at, 100);
    }

    #[test]
    fn publish_internal_flag_persists() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store
            .publish(&sample_event("ext", "src", 100), false)
            .unwrap();
        store
            .publish(&sample_event("int", "src", 100), true)
            .unwrap();
        assert!(!store.get("ext").unwrap().unwrap().internal);
        assert!(store.get("int").unwrap().unwrap().internal);
    }

    #[test]
    fn mark_dispatched_first_caller_claims_and_second_returns_false() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store
            .publish(&sample_event("evt-1", "ring", 100), false)
            .unwrap();
        let first = store.mark_dispatched("evt-1", 200).unwrap();
        assert!(first, "first mark_dispatched must return true (claim)");
        let first_ts = store.get("evt-1").unwrap().unwrap().dispatched_at.unwrap();

        let second = store.mark_dispatched("evt-1", 300).unwrap();
        assert!(
            !second,
            "second mark_dispatched must return false (already claimed)",
        );
        let second_ts = store.get("evt-1").unwrap().unwrap().dispatched_at.unwrap();
        assert_eq!(first_ts, 200);
        assert_eq!(second_ts, 200, "second mark must not overwrite the first");
    }

    #[test]
    fn fetch_pending_returns_only_undispatched_oldest_first() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store
            .publish(&sample_event("a", "src", 300), false)
            .unwrap();
        store
            .publish(&sample_event("b", "src", 100), false)
            .unwrap();
        store
            .publish(&sample_event("c", "src", 200), false)
            .unwrap();
        assert!(store.mark_dispatched("b", 1).unwrap());
        let ids = store.fetch_pending(false, 10).unwrap();
        assert_eq!(ids, vec!["c".to_string(), "a".to_string()]);
    }

    #[test]
    fn fetch_pending_respects_internal_only_filter() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store
            .publish(&sample_event("ext", "src", 100), false)
            .unwrap();
        store
            .publish(&sample_event("int", "src", 200), true)
            .unwrap();
        let all = store.fetch_pending(false, 10).unwrap();
        let internal = store.fetch_pending(true, 10).unwrap();
        assert_eq!(all, vec!["ext".to_string(), "int".to_string()]);
        assert_eq!(internal, vec!["int".to_string()]);
    }

    #[test]
    fn mark_dispatched_returns_false_for_unknown_id() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        // No row inserted â€” claim of a phantom id must return false
        // rather than create or error.
        let claimed = store.mark_dispatched("ghost", 100).unwrap();
        assert!(!claimed);
    }

    #[test]
    fn fetch_pending_respects_limit() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        for i in 0..5 {
            store
                .publish(&sample_event(&format!("e{i}"), "src", i as i64), false)
                .unwrap();
        }
        let ids = store.fetch_pending(false, 3).unwrap();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn purge_dispatched_older_than_skips_pending_rows() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store
            .publish(&sample_event("old-pending", "src", 100), false)
            .unwrap();
        store
            .publish(&sample_event("old-done", "src", 100), false)
            .unwrap();
        assert!(store.mark_dispatched("old-done", 100).unwrap());
        let n = store.purge_dispatched_older_than(1000).unwrap();
        assert_eq!(n, 1);
        // Pending row survives â€” retention must not paper over a
        // stuck dispatcher.
        assert!(store.get("old-pending").unwrap().is_some());
        assert!(store.get("old-done").unwrap().is_none());
    }

    #[test]
    fn purge_dispatched_respects_cutoff_boundary() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store
            .publish(&sample_event("just-before", "src", 99), false)
            .unwrap();
        store
            .publish(&sample_event("at-cutoff", "src", 100), false)
            .unwrap();
        store
            .publish(&sample_event("just-after", "src", 101), false)
            .unwrap();
        for id in ["just-before", "at-cutoff", "just-after"] {
            assert!(store.mark_dispatched(id, 1).unwrap());
        }
        // Cutoff is strict-less-than (rows AT the cutoff stay).
        let n = store.purge_dispatched_older_than(100).unwrap();
        assert_eq!(n, 1);
        assert!(store.get("just-before").unwrap().is_none());
        assert!(store.get("at-cutoff").unwrap().is_some());
        assert!(store.get("just-after").unwrap().is_some());
    }

    #[test]
    fn bus_event_kind_round_trip() {
        for k in [
            BusEventKind::WebhookReceived,
            BusEventKind::SocketMessage,
            BusEventKind::PluginEmit,
            BusEventKind::RoutineFired,
            BusEventKind::Other,
        ] {
            assert_eq!(BusEventKind::parse(k.as_str()), k);
        }
        // Unknown string falls into Other â€” the additive escape hatch
        // that keeps old binaries forward-compatible with new kinds.
        assert_eq!(BusEventKind::parse("future.kind"), BusEventKind::Other);
    }

    #[test]
    fn concurrent_publish_same_id_only_one_inserts() {
        // The PK is the dedup contract. If N threads race to publish
        // the same id, exactly one must see `Inserted` and the rest
        // must see `Duplicate`. The on-disk row is the FIRST writer's
        // payload â€” readers must never observe a partially-applied
        // overwrite.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let db = std::sync::Arc::new(fresh_db());
        let inserted = std::sync::Arc::new(AtomicUsize::new(0));
        let duplicate = std::sync::Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..16 {
            let db = db.clone();
            let inserted = inserted.clone();
            let duplicate = duplicate.clone();
            handles.push(std::thread::spawn(move || {
                let store = BusEventStore::new(&db);
                let evt = Event {
                    id: "shared".into(),
                    kind: BusEventKind::WebhookReceived,
                    source: format!("thread-{i}"),
                    received_at: i as i64,
                    payload: serde_json::json!({"thread": i}),
                    envelope: None,
                };
                match store.publish(&evt, false).unwrap() {
                    PublishOutcome::Inserted => inserted.fetch_add(1, Ordering::SeqCst),
                    PublishOutcome::Duplicate => duplicate.fetch_add(1, Ordering::SeqCst),
                };
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            inserted.load(Ordering::SeqCst),
            1,
            "exactly one INSERT wins"
        );
        assert_eq!(duplicate.load(Ordering::SeqCst), 15);
        // First writer's row is what's persisted â€” we can't predict
        // *which* thread won, but exactly one did. Spot-check the
        // source matches some thread-N pattern.
        let row = BusEventStore::new(&db).get("shared").unwrap().unwrap();
        assert!(row.source.starts_with("thread-"));
    }

    #[test]
    fn concurrent_publish_distinct_ids_all_land() {
        let db = std::sync::Arc::new(fresh_db());
        let mut handles = Vec::new();
        for i in 0..64 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                let store = BusEventStore::new(&db);
                let evt = Event {
                    id: format!("evt-{i}"),
                    kind: BusEventKind::WebhookReceived,
                    source: "stress".into(),
                    received_at: i as i64,
                    payload: serde_json::json!({"i": i}),
                    envelope: None,
                };
                assert_eq!(
                    store.publish(&evt, false).unwrap(),
                    PublishOutcome::Inserted
                );
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let pending = BusEventStore::new(&db).fetch_pending(false, 1000).unwrap();
        assert_eq!(pending.len(), 64);
    }

    #[test]
    fn concurrent_claim_only_one_wins() {
        // 32 threads race to mark_dispatched the same id. Exactly one
        // must return true; the rest return false. This is the
        // foundational guarantee the server-side `dispatch_one`
        // claim-before-handle invariant rides on.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let db = std::sync::Arc::new(fresh_db());
        BusEventStore::new(&db)
            .publish(&sample_event("contested", "src", 1), false)
            .unwrap();
        let claimed = std::sync::Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let db = db.clone();
            let claimed = claimed.clone();
            handles.push(std::thread::spawn(move || {
                let store = BusEventStore::new(&db);
                if store.mark_dispatched("contested", 100).unwrap() {
                    claimed.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            claimed.load(Ordering::SeqCst),
            1,
            "exactly one thread must win the claim",
        );
    }

    #[test]
    fn payload_round_trips_through_persistence() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        let evt = Event {
            id: "json-test".into(),
            kind: BusEventKind::PluginEmit,
            source: "plugin:weather".into(),
            received_at: 42,
            payload: serde_json::json!({
                "nested": {"k": [1, 2, 3]},
                "bool": true,
                "null": null,
                "string": "hello"
            }),
            envelope: None,
        };
        store.publish(&evt, false).unwrap();
        let row = store.get("json-test").unwrap().unwrap();
        assert_eq!(row.payload, evt.payload);
    }

    #[test]
    fn envelope_round_trips_through_persistence() {
        use crate::event_envelope::{EventEnvelope, OriginRef, SenderIdentity, TrustClass};

        let db = fresh_db();
        let store = BusEventStore::new(&db);
        let env = EventEnvelope {
            origin: OriginRef::PluginChannel {
                plugin_id: "whatsapp".into(),
                channel_ref: serde_json::json!({"chat_id": "+15551234"}),
                expires_at: Some(1_700_000_000_000),
            },
            identity: SenderIdentity::External {
                plugin_id: "whatsapp".into(),
                handle: "+15551234".into(),
                trust: TrustClass::ColdContact,
            },
            correlation_id: "corr-1".into(),
            parent_event_id: None,
        };
        let evt = Event {
            id: "env-test".into(),
            kind: BusEventKind::PluginEmit,
            source: "plugin:whatsapp".into(),
            received_at: 7,
            payload: serde_json::json!({"text": "hi"}),
            envelope: Some(env.clone()),
        };
        store.publish(&evt, false).unwrap();
        let row = store.get("env-test").unwrap().unwrap();
        assert_eq!(row.envelope, env);
    }

    #[test]
    fn legacy_row_without_envelope_decodes_as_system_internal() {
        use crate::event_envelope::{OriginRef, SenderIdentity};

        let db = fresh_db();
        let store = BusEventStore::new(&db);
        let evt = Event {
            id: "legacy".into(),
            kind: BusEventKind::WebhookReceived,
            source: "x".into(),
            received_at: 1,
            payload: serde_json::json!({}),
            envelope: None,
        };
        store.publish(&evt, false).unwrap();
        let row = store.get("legacy").unwrap().unwrap();
        assert!(matches!(row.envelope.origin, OriginRef::None));
        assert!(matches!(row.envelope.identity, SenderIdentity::System));
    }
}
