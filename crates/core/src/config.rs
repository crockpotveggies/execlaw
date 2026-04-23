//! Thin helpers over the `config_*` KV tables.
//!
//! These tables are used for free-form settings whose shape is validated at
//! the Rust layer rather than in the schema. Rich typed tables (runner
//! deployments, hardware overrides, etc.) live in dedicated modules.

use crate::db::{Database, DbError};
use rusqlite::params;

/// List of `config_*` KV tables we treat uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTable {
    TrustPolicy,
    AlertRouting,
    ResearchQuota,
    RuntimeSettings,
}

impl ConfigTable {
    fn name(&self) -> &'static str {
        match self {
            ConfigTable::TrustPolicy => "config_trust_policy",
            ConfigTable::AlertRouting => "config_alert_routing",
            ConfigTable::ResearchQuota => "config_research_quota",
            ConfigTable::RuntimeSettings => "config_runtime_settings",
        }
    }
}

pub struct ConfigKv<'db> {
    db: &'db Database,
    table: ConfigTable,
}

impl<'db> ConfigKv<'db> {
    pub fn new(db: &'db Database, table: ConfigTable) -> Self {
        Self { db, table }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), DbError> {
        let sql = format!(
            "INSERT INTO {}(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            self.table.name()
        );
        self.db.with_conn(|c| {
            c.execute(&sql, params![key, value])?;
            Ok(())
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, DbError> {
        let sql = format!("SELECT value FROM {} WHERE key = ?1", self.table.name());
        self.db.with_conn(|c| {
            let got = c
                .query_row(&sql, params![key], |r| r.get::<_, String>(0))
                .ok();
            Ok(got)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    #[test]
    fn kv_set_get_roundtrip() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let kv = ConfigKv::new(&db, ConfigTable::RuntimeSettings);
        kv.set("max_wakeups_per_hour", "12").unwrap();
        assert_eq!(kv.get("max_wakeups_per_hour").unwrap().as_deref(), Some("12"));
    }
}
