//! Alert row model and severity enum (§10).
//!
//! Routing (UI dropdown primary, Signal fallback) lives in `server` and
//! `transport-api` crates. This module only owns storage + basic CRUD.

use crate::db::{Database, DbError};
use crate::ids::{AlertId, IncidentId};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Critical,
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Critical => "Critical",
            Severity::Error => "Error",
            Severity::Warning => "Warning",
            Severity::Info => "Info",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Critical" => Some(Self::Critical),
            "Error" => Some(Self::Error),
            "Warning" => Some(Self::Warning),
            "Info" => Some(Self::Info),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertStatus {
    Firing,
    Acked,
    Resolved,
    Snoozed,
}

impl AlertStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertStatus::Firing => "Firing",
            AlertStatus::Acked => "Acked",
            AlertStatus::Resolved => "Resolved",
            AlertStatus::Snoozed => "Snoozed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Firing" => Some(Self::Firing),
            "Acked" => Some(Self::Acked),
            "Resolved" => Some(Self::Resolved),
            "Snoozed" => Some(Self::Snoozed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRow {
    pub id: AlertId,
    pub fingerprint: String,
    pub severity: Severity,
    pub source: String, // e.g. "plugin.google-calendar" or "core.outbox"
    pub title: String,
    pub detail: Option<String>,
    pub context_json: Option<Vec<u8>>, // JSON, not MessagePack, because
    // alert payloads want to be human-inspectable in the DB
    pub status: AlertStatus,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub occurrence_count: i64,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub ack_at: Option<i64>,
    pub ack_by: Option<String>,
    pub snooze_until: Option<i64>,
    pub incident_id: Option<IncidentId>,
    pub actions_json: Option<Vec<u8>>,
}

pub struct AlertStore<'db> {
    db: &'db Database,
}

impl<'db> AlertStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn insert_firing(&self, row: &AlertRow) -> Result<(), DbError> {
        // Dedup path: if a Firing row exists for the same fingerprint,
        // bump occurrence_count and last_seen_at instead of creating a new
        // row (§10.3).
        self.db.with_conn(|c| {
            let existing: Option<String> = c
                .query_row(
                    "SELECT id FROM state_alerts WHERE fingerprint = ?1 AND status = 'Firing'",
                    params![row.fingerprint],
                    |r| r.get::<_, String>(0),
                )
                .ok();
            if let Some(existing_id) = existing {
                c.execute(
                    "UPDATE state_alerts SET occurrence_count = occurrence_count + 1, \
                     last_seen_at = ?1 WHERE id = ?2",
                    params![row.last_seen_at, existing_id],
                )?;
            } else {
                c.execute(
                    "INSERT INTO state_alerts \
                     (id, fingerprint, severity, source, title, detail, context_json, status, \
                      first_seen_at, last_seen_at, occurrence_count, resolved_at, resolved_by, \
                      ack_at, ack_by, snooze_until, incident_id, actions_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL, NULL, \
                             NULL, NULL, ?12, ?13)",
                    params![
                        row.id.as_str(),
                        row.fingerprint,
                        row.severity.as_str(),
                        row.source,
                        row.title,
                        row.detail,
                        row.context_json,
                        row.status.as_str(),
                        row.first_seen_at,
                        row.last_seen_at,
                        row.occurrence_count,
                        row.incident_id.as_ref().map(|i| i.as_str().to_owned()),
                        row.actions_json,
                    ],
                )?;
            }
            Ok(())
        })
    }

    pub fn ack(&self, id: &AlertId, by: &str, at: i64) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_alerts SET status = 'Acked', ack_at = ?1, ack_by = ?2 WHERE id = ?3",
                params![at, by, id.as_str()],
            )?;
            Ok(())
        })
    }

    pub fn resolve(&self, id: &AlertId, by: &str, at: i64) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_alerts SET status = 'Resolved', resolved_at = ?1, resolved_by = ?2 \
                 WHERE id = ?3",
                params![at, by, id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Look up a single alert by id. Returns `Ok(None)` for unknown ids
    /// so callers can distinguish "doesn't exist" from "DB error."
    pub fn get(&self, id: &AlertId) -> Result<Option<AlertRow>, DbError> {
        let id_owned = id.as_str().to_owned();
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, fingerprint, severity, source, title, detail, context_json, \
                            status, first_seen_at, last_seen_at, occurrence_count, resolved_at, \
                            resolved_by, ack_at, ack_by, snooze_until, incident_id, actions_json \
                     FROM state_alerts WHERE id = ?1",
                    params![id_owned],
                    row_to_alert,
                )
                .ok();
            Ok(row)
        })
    }

    /// List alerts in last-seen-descending order. Optional filters:
    ///   * `status_in` — restrict to a status set; pass `None` to
    ///     include every status (Firing/Acked/Resolved/Snoozed).
    ///   * `limit` — cap the result count; `None` returns the full
    ///     table (small in practice — `state_alerts` is dedup'd by
    ///     fingerprint, not append-only per occurrence).
    pub fn list(
        &self,
        status_in: Option<&[AlertStatus]>,
        limit: Option<u32>,
    ) -> Result<Vec<AlertRow>, DbError> {
        let mut sql = String::from(
            "SELECT id, fingerprint, severity, source, title, detail, context_json, \
                    status, first_seen_at, last_seen_at, occurrence_count, resolved_at, \
                    resolved_by, ack_at, ack_by, snooze_until, incident_id, actions_json \
             FROM state_alerts",
        );
        let status_strs: Vec<String> = status_in
            .map(|ss| ss.iter().map(|s| s.as_str().to_owned()).collect())
            .unwrap_or_default();
        if !status_strs.is_empty() {
            sql.push_str(" WHERE status IN (");
            for (i, _) in status_strs.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push('?');
                sql.push_str(&(i + 1).to_string());
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY last_seen_at DESC");
        if let Some(l) = limit {
            sql.push_str(&format!(" LIMIT {l}"));
        }

        self.db.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let params_vec: Vec<&dyn rusqlite::ToSql> = status_strs
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params_vec.iter()), row_to_alert)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Count of alerts in `Firing` status — used by the SPA badge so
    /// the operator notices an active alert without having to open
    /// the page.
    pub fn count_firing(&self) -> Result<i64, DbError> {
        self.db.with_conn(|c| {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM state_alerts WHERE status = 'Firing'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok(n)
        })
    }

    /// Return the id of an existing firing row matching this
    /// fingerprint, if any. Used by callers that want to report
    /// "did this dedup against an existing alert?" without doing
    /// the extra row probe themselves. The reverse semantic of
    /// `insert_firing` (which silently bumps the occurrence count
    /// instead of telling you whether it deduped).
    pub fn firing_id_for_fingerprint(&self, fingerprint: &str) -> Result<Option<String>, DbError> {
        self.db.with_conn(|c| {
            let got: Option<String> = c
                .query_row(
                    "SELECT id FROM state_alerts WHERE fingerprint = ?1 AND status = 'Firing'",
                    params![fingerprint],
                    |r| r.get::<_, String>(0),
                )
                .ok();
            Ok(got)
        })
    }
}

fn row_to_alert(row: &rusqlite::Row<'_>) -> rusqlite::Result<AlertRow> {
    let id_str: String = row.get(0)?;
    let severity_str: String = row.get(2)?;
    let status_str: String = row.get(7)?;
    let incident_id_str: Option<String> = row.get(16)?;

    let severity = Severity::parse(&severity_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown severity: {severity_str}"),
            )),
        )
    })?;
    let status = AlertStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown alert status: {status_str}"),
            )),
        )
    })?;

    Ok(AlertRow {
        id: AlertId::from(id_str),
        fingerprint: row.get(1)?,
        severity,
        source: row.get(3)?,
        title: row.get(4)?,
        detail: row.get(5)?,
        context_json: row.get(6)?,
        status,
        first_seen_at: row.get(8)?,
        last_seen_at: row.get(9)?,
        occurrence_count: row.get(10)?,
        resolved_at: row.get(11)?,
        resolved_by: row.get(12)?,
        ack_at: row.get(13)?,
        ack_by: row.get(14)?,
        snooze_until: row.get(15)?,
        incident_id: incident_id_str.map(IncidentId::from),
        actions_json: row.get(17)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn mk_row(fp: &str) -> AlertRow {
        AlertRow {
            id: AlertId::new(),
            fingerprint: fp.into(),
            severity: Severity::Error,
            source: "core.test".into(),
            title: "something broke".into(),
            detail: None,
            context_json: None,
            status: AlertStatus::Firing,
            first_seen_at: 100,
            last_seen_at: 100,
            occurrence_count: 1,
            resolved_at: None,
            resolved_by: None,
            ack_at: None,
            ack_by: None,
            snooze_until: None,
            incident_id: None,
            actions_json: None,
        }
    }

    #[test]
    fn firing_dedup_increments_count() {
        let db = fresh_db();
        let store = AlertStore::new(&db);
        let a = mk_row("test:flake:1");
        store.insert_firing(&a).unwrap();
        store.insert_firing(&a).unwrap();
        store.insert_firing(&a).unwrap();
        let count: i64 = db
            .with_conn(|c| {
                let v: i64 = c
                    .query_row(
                        "SELECT occurrence_count FROM state_alerts WHERE fingerprint = ?1",
                        params![a.fingerprint],
                        |r| r.get(0),
                    )
                    .unwrap();
                Ok(v)
            })
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn list_returns_alerts_in_last_seen_desc_order() {
        let db = fresh_db();
        let store = AlertStore::new(&db);

        let mut older = mk_row("alert-older");
        older.last_seen_at = 100;
        store.insert_firing(&older).unwrap();

        let mut newer = mk_row("alert-newer");
        newer.last_seen_at = 200;
        store.insert_firing(&newer).unwrap();

        let rows = store.list(None, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fingerprint, "alert-newer");
        assert_eq!(rows[1].fingerprint, "alert-older");
    }

    #[test]
    fn list_filters_by_status_set() {
        let db = fresh_db();
        let store = AlertStore::new(&db);

        let firing = mk_row("alert-1");
        store.insert_firing(&firing).unwrap();
        let acked_src = mk_row("alert-2");
        store.insert_firing(&acked_src).unwrap();
        store.ack(&acked_src.id, "ctrl", 150).unwrap();

        let firing_only = store.list(Some(&[AlertStatus::Firing]), None).unwrap();
        assert_eq!(firing_only.len(), 1);
        assert_eq!(firing_only[0].fingerprint, "alert-1");

        let acked_only = store.list(Some(&[AlertStatus::Acked]), None).unwrap();
        assert_eq!(acked_only.len(), 1);
        assert_eq!(acked_only[0].fingerprint, "alert-2");

        let either = store
            .list(Some(&[AlertStatus::Firing, AlertStatus::Acked]), None)
            .unwrap();
        assert_eq!(either.len(), 2);
    }

    #[test]
    fn list_limit_truncates_results() {
        let db = fresh_db();
        let store = AlertStore::new(&db);
        for i in 0..5 {
            let mut r = mk_row(&format!("alert-{i}"));
            r.last_seen_at = 100 + i;
            store.insert_firing(&r).unwrap();
        }
        let rows = store.list(None, Some(2)).unwrap();
        assert_eq!(rows.len(), 2);
        // Most-recent first: alert-4, alert-3.
        assert_eq!(rows[0].fingerprint, "alert-4");
        assert_eq!(rows[1].fingerprint, "alert-3");
    }

    #[test]
    fn count_firing_excludes_acked_and_resolved() {
        let db = fresh_db();
        let store = AlertStore::new(&db);
        let a = mk_row("a");
        let b = mk_row("b");
        let c = mk_row("c");
        store.insert_firing(&a).unwrap();
        store.insert_firing(&b).unwrap();
        store.insert_firing(&c).unwrap();
        assert_eq!(store.count_firing().unwrap(), 3);
        store.ack(&a.id, "ctrl", 100).unwrap();
        store.resolve(&b.id, "ctrl", 100).unwrap();
        assert_eq!(store.count_firing().unwrap(), 1);
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let db = fresh_db();
        let store = AlertStore::new(&db);
        let unknown = AlertId::new();
        assert!(store.get(&unknown).unwrap().is_none());
    }

    #[test]
    fn get_round_trips_a_full_row() {
        let db = fresh_db();
        let store = AlertStore::new(&db);
        let a = mk_row("trip-1");
        store.insert_firing(&a).unwrap();
        let got = store.get(&a.id).unwrap().expect("row exists");
        assert_eq!(got.fingerprint, "trip-1");
        assert_eq!(got.severity, Severity::Error);
        assert_eq!(got.status, AlertStatus::Firing);
        assert_eq!(got.source, "core.test");
    }

    #[test]
    fn ack_and_resolve_update_row() {
        let db = fresh_db();
        let store = AlertStore::new(&db);
        let a = mk_row("test:ack:1");
        store.insert_firing(&a).unwrap();
        store.ack(&a.id, "controller", 200).unwrap();
        store.resolve(&a.id, "controller", 300).unwrap();
        let (status, ack_at, resolved_at): (String, Option<i64>, Option<i64>) = db
            .with_conn(|c| {
                let v: (String, Option<i64>, Option<i64>) = c
                    .query_row(
                        "SELECT status, ack_at, resolved_at FROM state_alerts WHERE id = ?1",
                        params![a.id.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .unwrap();
                Ok(v)
            })
            .unwrap();
        assert_eq!(status, "Resolved");
        assert_eq!(ack_at, Some(200));
        assert_eq!(resolved_at, Some(300));
    }
}
