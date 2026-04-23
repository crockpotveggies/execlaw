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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

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
}
