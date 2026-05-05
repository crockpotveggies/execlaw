//! `log_entries` row (§2.12). The actual tracing subscriber that writes
//! rows lives in `server` or wherever the process is assembled. This
//! module just offers a direct-insert helper for tests / the CLI.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRow {
    pub ts_ms: i64,
    pub level: LogLevel,
    pub target: String,
    pub conversation_id: Option<String>,
    pub plugin_id: Option<String>,
    pub message: String,
    pub fields_json: Option<Vec<u8>>,
}

pub struct LogStore<'db> {
    db: &'db Database,
}

impl<'db> LogStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn insert(&self, row: &LogRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO log_entries(ts_ms, level, target, conversation_id, plugin_id, \
                                         message, fields_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.ts_ms,
                    row.level.as_str(),
                    row.target,
                    row.conversation_id,
                    row.plugin_id,
                    row.message,
                    row.fields_json,
                ],
            )?;
            Ok(())
        })
    }

    /// Query rows with optional filters. `since_ms` is inclusive
    /// (entries with `ts_ms >= since_ms` come back). Results are
    /// ordered by `ts_ms` descending — newest first.
    pub fn query(
        &self,
        level: Option<LogLevel>,
        plugin_id: Option<&str>,
        conversation_id: Option<&str>,
        since_ms: Option<i64>,
        limit: i64,
    ) -> Result<Vec<LogRow>, DbError> {
        let mut sql = String::from(
            "SELECT ts_ms, level, target, conversation_id, plugin_id, message, fields_json \
             FROM log_entries WHERE 1=1",
        );
        if level.is_some() {
            sql.push_str(" AND level = ?");
        }
        if plugin_id.is_some() {
            sql.push_str(" AND plugin_id = ?");
        }
        if conversation_id.is_some() {
            sql.push_str(" AND conversation_id = ?");
        }
        if since_ms.is_some() {
            sql.push_str(" AND ts_ms >= ?");
        }
        sql.push_str(" ORDER BY ts_ms DESC LIMIT ?");

        self.db.with_conn(|c| {
            // Build params list dynamically. rusqlite needs a fixed
            // type so we go through the enum-typed params! macro.
            let level_str = level.map(|l| l.as_str().to_owned());
            let level_ref: Option<&str> = level_str.as_deref();

            let mut stmt = c.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
            if let Some(l) = &level_ref {
                binds.push(l);
            }
            if let Some(p) = &plugin_id {
                binds.push(p);
            }
            if let Some(cv) = &conversation_id {
                binds.push(cv);
            }
            if let Some(s) = &since_ms {
                binds.push(s);
            }
            binds.push(&limit);

            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter().copied()), |r| {
                    let ts_ms: i64 = r.get(0)?;
                    let lvl_str: String = r.get(1)?;
                    let target: String = r.get(2)?;
                    let conv: Option<String> = r.get(3)?;
                    let plugin: Option<String> = r.get(4)?;
                    let message: String = r.get(5)?;
                    let fields: Option<Vec<u8>> = r.get(6)?;
                    let level = match lvl_str.as_str() {
                        "TRACE" => LogLevel::Trace,
                        "DEBUG" => LogLevel::Debug,
                        "INFO" => LogLevel::Info,
                        "WARN" => LogLevel::Warn,
                        _ => LogLevel::Error,
                    };
                    Ok(LogRow {
                        ts_ms,
                        level,
                        target,
                        conversation_id: conv,
                        plugin_id: plugin,
                        message,
                        fields_json: fields,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Delete every row whose `ts_ms < cutoff_ms`. Returns the number
    /// of rows removed. Powers the Phase-7 retention sweeper.
    pub fn purge_older_than(&self, cutoff_ms: i64) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM log_entries WHERE ts_ms < ?1",
                params![cutoff_ms],
            )?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    #[test]
    fn query_filters_by_level_plugin_and_since() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = LogStore::new(&db);

        // Three rows: different levels, different plugin scope, different times.
        for (ts, level, plugin) in [
            (100, LogLevel::Info, None),
            (200, LogLevel::Warn, Some("plugin-signal".to_owned())),
            (300, LogLevel::Error, Some("plugin-signal".to_owned())),
        ] {
            store
                .insert(&LogRow {
                    ts_ms: ts,
                    level,
                    target: "test".into(),
                    conversation_id: None,
                    plugin_id: plugin,
                    message: format!("msg @ {ts}"),
                    fields_json: None,
                })
                .unwrap();
        }

        // Filter by level → only the WARN.
        let warns = store
            .query(Some(LogLevel::Warn), None, None, None, 100)
            .unwrap();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].ts_ms, 200);

        // Filter by plugin_id → 2 rows (warn + error).
        let plugin_rows = store
            .query(None, Some("plugin-signal"), None, None, 100)
            .unwrap();
        assert_eq!(plugin_rows.len(), 2);

        // Filter by since_ms = 250 → only the error.
        let recent = store.query(None, None, None, Some(250), 100).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].ts_ms, 300);

        // Limit truncates.
        let limited = store.query(None, None, None, None, 1).unwrap();
        assert_eq!(limited.len(), 1);

        // Default ordering: newest first.
        let all = store.query(None, None, None, None, 100).unwrap();
        assert_eq!(all[0].ts_ms, 300);
        assert_eq!(all[2].ts_ms, 100);
    }

    #[test]
    fn log_insert_and_fetch() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = LogStore::new(&db);
        store
            .insert(&LogRow {
                ts_ms: 100,
                level: LogLevel::Info,
                target: "test".into(),
                conversation_id: None,
                plugin_id: None,
                message: "hello".into(),
                fields_json: None,
            })
            .unwrap();

        let n: i64 = db
            .with_conn(|c| {
                let v: i64 = c.query_row("SELECT COUNT(*) FROM log_entries", [], |r| r.get(0))?;
                Ok(v)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    fn count_rows(db: &Database) -> i64 {
        db.with_conn(|c| {
            let v: i64 = c.query_row("SELECT COUNT(*) FROM log_entries", [], |r| r.get(0))?;
            Ok(v)
        })
        .unwrap()
    }

    #[test]
    fn purge_removes_old_rows_only_and_reports_count() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = LogStore::new(&db);
        for ts in [100, 200, 300, 400] {
            store
                .insert(&LogRow {
                    ts_ms: ts,
                    level: LogLevel::Info,
                    target: "t".into(),
                    conversation_id: None,
                    plugin_id: None,
                    message: format!("@{ts}"),
                    fields_json: None,
                })
                .unwrap();
        }
        // Cutoff < first ts → nothing dropped.
        assert_eq!(store.purge_older_than(50).unwrap(), 0);
        assert_eq!(count_rows(&db), 4);
        // Cutoff = 250 drops 100 and 200 (strictly less than).
        assert_eq!(store.purge_older_than(250).unwrap(), 2);
        assert_eq!(count_rows(&db), 2);
        // Re-running with same cutoff is a no-op (idempotent).
        assert_eq!(store.purge_older_than(250).unwrap(), 0);
        // Boundary: cutoff equal to a row's ts_ms preserves that row.
        assert_eq!(store.purge_older_than(300).unwrap(), 0);
        assert_eq!(count_rows(&db), 2);
    }

    #[test]
    fn purge_on_empty_table_returns_zero() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = LogStore::new(&db);
        assert_eq!(store.purge_older_than(0).unwrap(), 0);
        assert_eq!(store.purge_older_than(i64::MAX).unwrap(), 0);
    }
}
