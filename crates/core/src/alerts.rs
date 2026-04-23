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
