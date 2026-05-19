//! Automation suggestions — the discovery surface for the
//! `/automations` landing page (M4).
//!
//! The daily sweep scans `state_bus_events`, groups by `(kind,
//! source)`, and surfaces patterns that meet all of:
//!
//!   * Window: events arrived within the last `SWEEP_WINDOW_DAYS`.
//!   * Threshold: ≥ `MIN_EVENT_COUNT` distinct events.
//!   * No matching enabled automation for the kind that would have
//!     consumed those events.
//!   * The `(kind, source)` is not in `state_automation_muted_patterns`.
//!
//! For each surviving pattern, the sweep upserts a `pending` row in
//! `state_automation_suggestions`. The unique index on
//! `(kind, source, status)` keeps the sweep idempotent — re-running
//! updates the existing row instead of duplicating.
//!
//! The agent-drafted variant (M5) plugs in at the same seam: it
//! reads the pending rows, asks the model to propose a draft graph,
//! and stuffs the proposal into a follow-on column.

use crate::automation_bus::BusEventKind;
use crate::automations::AutomationStore;
use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

/// Default sweep cadence — once per day. Suggestions are a discovery
/// surface, not an alerting one; daily is plenty of freshness.
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Window of bus events the sweep considers when computing counts.
/// 7 days matches the design doc; longer windows capture rare but
/// real patterns without diluting the "high volume" signal.
pub const SWEEP_WINDOW_DAYS: i64 = 7;

/// Threshold: a `(kind, source)` pattern must produce at least this
/// many events in the window to be worth suggesting. Conservative —
/// noise-floor protection.
pub const MIN_EVENT_COUNT: i64 = 10;

/// Per-suggestion sample-event cap. We carry the first N event IDs
/// (oldest in the window) so the editor can let the operator
/// "use as test payload" without a follow-on query.
pub const SAMPLE_EVENT_CAP: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionStatus {
    /// Active suggestion — surfaces on the landing page.
    Pending,
    /// Operator dismissed it. The `(kind, source)` is also written
    /// into `state_automation_muted_patterns` so future sweeps skip.
    Dismissed,
    /// Operator clicked through to the editor and created an
    /// automation. We retain the historical row for telemetry but
    /// hide it from the suggestions list.
    Actioned,
}

impl SuggestionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dismissed => "dismissed",
            Self::Actioned => "actioned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "dismissed" => Some(Self::Dismissed),
            "actioned" => Some(Self::Actioned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestionRow {
    pub id: String,
    pub kind: BusEventKind,
    pub source: String,
    pub event_count: i64,
    pub sample_event_ids: Vec<String>,
    pub suggested_name: String,
    pub status: SuggestionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutedPatternRow {
    pub kind: BusEventKind,
    pub source: String,
    pub muted_at: i64,
}

#[derive(Debug, Error)]
pub enum SuggestionError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

pub struct SuggestionStore<'a> {
    db: &'a Database,
}

impl<'a> SuggestionStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Run one sweep pass. Returns the number of suggestion rows
    /// upserted (created OR refreshed). Pure-ish — `now_unix` is
    /// caller-supplied for test determinism.
    pub fn sweep(&self, now_unix: i64) -> Result<usize, SuggestionError> {
        let window_start = now_unix.saturating_sub(SWEEP_WINDOW_DAYS * 86_400);
        let candidates = self.collect_candidates(window_start)?;
        let muted = self.muted_pairs()?;
        let auto_store = AutomationStore::new(self.db);
        // Cache enabled-kinds so we don't re-query per candidate.
        let mut active_kinds: HashSet<String> = HashSet::new();
        for k in [
            BusEventKind::WebhookReceived,
            BusEventKind::SocketMessage,
            BusEventKind::PluginEmit,
            BusEventKind::RoutineFired,
        ] {
            if !auto_store
                .list_enabled_for_kind(k)
                .map_err(|e| SuggestionError::Db(DbError::Migration(format!("{e}"))))?
                .is_empty()
            {
                active_kinds.insert(k.as_str().to_string());
            }
        }
        let mut written = 0;
        for cand in candidates {
            if muted.contains(&(cand.kind, cand.source.clone())) {
                continue;
            }
            // Skip the pattern if the operator already has an enabled
            // automation for this kind. Strictly speaking we'd also
            // want a per-source match, but in practice a kind-level
            // match is the right signal — once you have a Ring
            // automation, the bus's ring webhooks are no longer
            // "untriaged" from the operator's perspective.
            if active_kinds.contains(cand.kind.as_str()) {
                continue;
            }
            self.upsert_pending(&cand, now_unix)?;
            written += 1;
        }
        Ok(written)
    }

    /// Gather `(kind, source) -> (count, first_N_event_ids)` over the
    /// window. Returns one [`Candidate`] per pattern that meets the
    /// `MIN_EVENT_COUNT` threshold.
    pub fn collect_candidates(
        &self,
        window_start_unix: i64,
    ) -> Result<Vec<Candidate>, SuggestionError> {
        // received_at on the bus is millis (per the design doc); but
        // for safety against accidental seconds inputs the window
        // boundary is multiplied by 1000 inline below. We accept both
        // input shapes by comparing against a millis cutoff.
        let window_start_ms = window_start_unix.saturating_mul(1000);
        let mut by_pair: std::collections::BTreeMap<(String, String), (i64, Vec<String>)> =
            Default::default();
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, received_at \
                 FROM state_bus_events \
                 WHERE received_at >= ?1 \
                 ORDER BY received_at ASC",
            )?;
            let rows = stmt.query_map(params![window_start_ms], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, kind, source) = row?;
                let entry = by_pair.entry((kind, source)).or_insert_with(|| (0, Vec::new()));
                entry.0 += 1;
                if entry.1.len() < SAMPLE_EVENT_CAP {
                    entry.1.push(id);
                }
            }
            Ok(())
        })?;
        let mut out = Vec::new();
        for ((kind, source), (count, samples)) in by_pair {
            if count >= MIN_EVENT_COUNT {
                out.push(Candidate {
                    kind: BusEventKind::parse(&kind),
                    source,
                    event_count: count,
                    sample_event_ids: samples,
                });
            }
        }
        // Stable order — biggest first so the editor lands the most
        // impactful suggestion at the top.
        out.sort_by(|a, b| b.event_count.cmp(&a.event_count));
        Ok(out)
    }

    fn muted_pairs(&self) -> Result<HashSet<(BusEventKind, String)>, SuggestionError> {
        let mut out = HashSet::new();
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, source FROM state_automation_muted_patterns",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                ))
            })?;
            for row in rows {
                let (kind, source) = row?;
                out.insert((BusEventKind::parse(&kind), source));
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn upsert_pending(
        &self,
        cand: &Candidate,
        now: i64,
    ) -> Result<(), SuggestionError> {
        let id = Uuid::new_v4().to_string();
        let kind_str = cand.kind.as_str();
        let samples = serde_json::to_string(&cand.sample_event_ids)?;
        let suggested_name = derive_suggested_name(cand.kind, &cand.source);
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_automation_suggestions \
                 (id, kind, source, event_count, sample_event_ids, suggested_name, \
                  status, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7) \
                 ON CONFLICT(kind, source, status) DO UPDATE SET \
                   event_count = excluded.event_count, \
                   sample_event_ids = excluded.sample_event_ids, \
                   suggested_name = excluded.suggested_name, \
                   updated_at = excluded.updated_at",
                params![
                    &id,
                    kind_str,
                    &cand.source,
                    cand.event_count,
                    &samples,
                    &suggested_name,
                    now,
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn list_pending(&self) -> Result<Vec<SuggestionRow>, SuggestionError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, event_count, sample_event_ids, suggested_name, \
                        status, created_at, updated_at \
                 FROM state_automation_suggestions \
                 WHERE status = 'pending' \
                 ORDER BY event_count DESC, updated_at DESC",
            )?;
            let rows = stmt.query_map([], row_to_suggestion)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> Result<Option<SuggestionRow>, SuggestionError> {
        let row = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, event_count, sample_event_ids, suggested_name, \
                        status, created_at, updated_at \
                 FROM state_automation_suggestions WHERE id = ?1",
            )?;
            let r = stmt.query_row([id], row_to_suggestion).ok();
            Ok(r)
        })?;
        Ok(row)
    }

    /// Dismiss a suggestion. Flips status to `dismissed` AND inserts
    /// the `(kind, source)` into `state_automation_muted_patterns` so
    /// future sweeps skip it.
    pub fn dismiss(&self, id: &str, now: i64) -> Result<bool, SuggestionError> {
        let row = match self.get(id)? {
            Some(r) => r,
            None => return Ok(false),
        };
        if !matches!(row.status, SuggestionStatus::Pending) {
            return Ok(false);
        }
        self.db.with_conn(|c| {
            // The unique index includes status, so flipping it
            // out of `pending` frees the slot for a future re-sweep
            // (if the operator un-mutes).
            c.execute(
                "UPDATE state_automation_suggestions \
                 SET status = 'dismissed', updated_at = ?2 \
                 WHERE id = ?1",
                params![id, now],
            )?;
            c.execute(
                "INSERT INTO state_automation_muted_patterns (kind, source, muted_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(kind, source) DO UPDATE SET muted_at = excluded.muted_at",
                params![row.kind.as_str(), &row.source, now],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    /// Mark a suggestion as actioned. Called by the API when the
    /// operator creates an automation from the suggestion's template.
    pub fn mark_actioned(&self, id: &str, now: i64) -> Result<bool, SuggestionError> {
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_automation_suggestions \
                 SET status = 'actioned', updated_at = ?2 \
                 WHERE id = ?1 AND status = 'pending'",
                params![id, now],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    pub fn list_muted(&self) -> Result<Vec<MutedPatternRow>, SuggestionError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, source, muted_at FROM state_automation_muted_patterns \
                 ORDER BY muted_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(MutedPatternRow {
                    kind: BusEventKind::parse(&r.get::<_, String>(0)?),
                    source: r.get(1)?,
                    muted_at: r.get(2)?,
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

    /// Un-mute a pattern. Called by the (future) settings UI; also
    /// useful for tests.
    pub fn unmute(&self, kind: BusEventKind, source: &str) -> Result<bool, SuggestionError> {
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_automation_muted_patterns WHERE kind = ?1 AND source = ?2",
                params![kind.as_str(), source],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: BusEventKind,
    pub source: String,
    pub event_count: i64,
    pub sample_event_ids: Vec<String>,
}

/// Default name for a sweep-generated suggestion. Operators rename
/// in the editor; this is the placeholder.
fn derive_suggested_name(kind: BusEventKind, source: &str) -> String {
    // Strip the namespace prefix from sources like `webhook:ring` →
    // `ring` so the displayed name reads naturally.
    let short_source = source.split(':').last().unwrap_or(source);
    match kind {
        BusEventKind::WebhookReceived => format!("Automate {} webhook", short_source),
        BusEventKind::SocketMessage => format!("Automate {} message", short_source),
        BusEventKind::PluginEmit => format!("Automate {} event", short_source),
        BusEventKind::RoutineFired => format!("React to {} routine", short_source),
        BusEventKind::Other => format!("Automate {} event", short_source),
    }
}

fn row_to_suggestion(r: &rusqlite::Row<'_>) -> rusqlite::Result<SuggestionRow> {
    let samples_str: String = r.get(4)?;
    let sample_event_ids: Vec<String> = serde_json::from_str(&samples_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status_str: String = r.get(6)?;
    let status = SuggestionStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            format!("unknown suggestion status: {status_str}").into(),
        )
    })?;
    Ok(SuggestionRow {
        id: r.get(0)?,
        kind: BusEventKind::parse(&r.get::<_, String>(1)?),
        source: r.get(2)?,
        event_count: r.get(3)?,
        sample_event_ids,
        suggested_name: r.get(5)?,
        status,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation_bus::{BusEventStore, Event as BusEvent};
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_events(db: &Database, source: &str, count: i64, received_at_ms: i64) {
        let store = BusEventStore::new(db);
        for i in 0..count {
            store
                .publish(
                    &BusEvent {
                        id: format!("{source}-{i}"),
                        kind: BusEventKind::WebhookReceived,
                        source: source.into(),
                        received_at: received_at_ms + i,
                        payload: serde_json::json!({}),
                    },
                    false,
                )
                .unwrap();
        }
    }

    #[test]
    fn sweep_surfaces_high_volume_pattern_with_no_matching_automation() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        // Seed 15 events from `ring` at "now" — passes threshold.
        let now = 1_000_000;
        seed_events(&db, "webhook:ring", 15, now * 1000);
        let written = store.sweep(now).unwrap();
        assert_eq!(written, 1);
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source, "webhook:ring");
        assert_eq!(pending[0].event_count, 15);
        assert_eq!(pending[0].sample_event_ids.len(), SAMPLE_EVENT_CAP);
        assert!(pending[0].suggested_name.contains("ring"));
    }

    #[test]
    fn sweep_skips_low_volume_pattern() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        // 9 events — under the threshold of 10.
        seed_events(&db, "webhook:slow", 9, now * 1000);
        let written = store.sweep(now).unwrap();
        assert_eq!(written, 0);
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn sweep_skips_pattern_outside_window() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 10_000_000;
        // Old events: 30 days ago. Past the 7-day window.
        let old_ms = (now - 30 * 86_400) * 1000;
        seed_events(&db, "webhook:old", 20, old_ms);
        let written = store.sweep(now).unwrap();
        assert_eq!(written, 0);
    }

    #[test]
    fn sweep_skips_muted_patterns() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        seed_events(&db, "webhook:noisy", 15, now * 1000);
        // Mute the pattern up front.
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_automation_muted_patterns (kind, source, muted_at) \
                 VALUES ('webhook.received', 'webhook:noisy', ?1)",
                params![now],
            )?;
            Ok(())
        })
        .unwrap();
        let written = store.sweep(now).unwrap();
        assert_eq!(written, 0);
    }

    #[test]
    fn sweep_skips_kinds_with_an_enabled_automation() {
        use crate::automations::{
            AutomationDef, AutomationStore, AutomationUpsert, EdgeDef, NodeDef, NodeKind, TriggerDef,
            END_SENTINEL, TRIGGER_SENTINEL,
        };
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        seed_events(&db, "webhook:ring", 15, now * 1000);
        // Operator already has an enabled automation for webhook.received.
        let auto_store = AutomationStore::new(&db);
        auto_store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "existing".into(),
                    enabled: true,
                    definition: AutomationDef {
                        trigger: TriggerDef {
                            kind: BusEventKind::WebhookReceived,
                            when: None,
                        },
                        nodes: vec![NodeDef {
                            id: "end".into(),
                            kind: NodeKind::Terminal,
                            config: serde_json::json!({}),
                        }],
                        edges: vec![EdgeDef {
                            from: TRIGGER_SENTINEL.into(),
                            to: "end".into(),
                            when: None,
                        }],
                    },
                },
                now,
            )
            .unwrap();
        let written = store.sweep(now).unwrap();
        assert_eq!(written, 0, "kind with existing automation must not produce suggestions");
    }

    #[test]
    fn sweep_is_idempotent_and_refreshes_count() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        // First pass: 15 events.
        seed_events(&db, "webhook:burst", 15, now * 1000);
        store.sweep(now).unwrap();
        // Add 10 more.
        seed_events(&db, "webhook:burst", 10, now * 1000 + 100);
        // SQLite: the duplicate event-id seeding would fail. Use
        // distinct ids by passing a fresh offset prefix instead.
        let bus = BusEventStore::new(&db);
        for i in 100..120 {
            bus.publish(
                &BusEvent {
                    id: format!("webhook:burst-{i}"),
                    kind: BusEventKind::WebhookReceived,
                    source: "webhook:burst".into(),
                    received_at: now * 1000 + i,
                    payload: serde_json::json!({}),
                },
                false,
            )
            .unwrap();
        }
        store.sweep(now + 1).unwrap();
        let pending = store.list_pending().unwrap();
        // Still one suggestion — refreshed in place.
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0].event_count >= 35,
            "refreshed count should reflect all events; got {}",
            pending[0].event_count,
        );
        assert!(pending[0].updated_at > pending[0].created_at);
    }

    #[test]
    fn dismiss_mutes_pattern_and_blocks_future_sweep() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        seed_events(&db, "webhook:annoying", 15, now * 1000);
        store.sweep(now).unwrap();
        let pending = store.list_pending().unwrap();
        let id = pending[0].id.clone();
        assert!(store.dismiss(&id, now + 1).unwrap());
        // Status is now dismissed.
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, SuggestionStatus::Dismissed);
        assert!(store.list_pending().unwrap().is_empty());
        // Mute row exists.
        let muted = store.list_muted().unwrap();
        assert_eq!(muted.len(), 1);
        // Re-sweep — must not resurface the dismissed pattern.
        let bus = BusEventStore::new(&db);
        for i in 100..110 {
            bus.publish(
                &BusEvent {
                    id: format!("webhook:annoying-extra-{i}"),
                    kind: BusEventKind::WebhookReceived,
                    source: "webhook:annoying".into(),
                    received_at: now * 1000 + 1000 + i,
                    payload: serde_json::json!({}),
                },
                false,
            )
            .unwrap();
        }
        let written = store.sweep(now + 2).unwrap();
        assert_eq!(written, 0);
    }

    #[test]
    fn mark_actioned_only_transitions_from_pending() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        seed_events(&db, "webhook:auto", 15, now * 1000);
        store.sweep(now).unwrap();
        let id = store.list_pending().unwrap()[0].id.clone();
        assert!(store.mark_actioned(&id, now + 1).unwrap());
        assert!(!store.mark_actioned(&id, now + 2).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, SuggestionStatus::Actioned);
    }

    #[test]
    fn unmute_removes_pattern() {
        let db = fresh_db();
        let store = SuggestionStore::new(&db);
        let now = 1_000_000;
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_automation_muted_patterns (kind, source, muted_at) \
                 VALUES ('webhook.received', 'x', ?1)",
                params![now],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(store.unmute(BusEventKind::WebhookReceived, "x").unwrap());
        assert!(store.list_muted().unwrap().is_empty());
        // Idempotent.
        assert!(!store.unmute(BusEventKind::WebhookReceived, "x").unwrap());
    }

    #[test]
    fn suggested_name_derives_from_source_short_form() {
        assert_eq!(
            derive_suggested_name(BusEventKind::WebhookReceived, "webhook:ring"),
            "Automate ring webhook",
        );
        assert_eq!(
            derive_suggested_name(BusEventKind::RoutineFired, "routine:morning-digest"),
            "React to morning-digest routine",
        );
        assert_eq!(
            derive_suggested_name(BusEventKind::PluginEmit, "plugin:weather"),
            "Automate weather event",
        );
    }
}
