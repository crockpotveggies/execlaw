//! Phase 14 — singleton-row store for operator-editable "general"
//! settings (start-on-boot toggle, bind address, …). One row per DB,
//! seeded by migration `0017_general_settings.sql`.
//!
//! The store is a thin wrapper around `Database` so callers (the
//! `/api/admin/settings/general` route + `cli/main.rs::cmd_install`)
//! don't have to spell out the SQL every time.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralSettings {
    /// Whether the host service should start at OS boot. Migrated to
    /// `service-manager`'s `autostart` flag on `execlaw install` /
    /// `execlaw service install`.
    pub start_on_boot: bool,
    /// `host:port` the service listens on. Default `127.0.0.1:3030`.
    /// Edit takes effect on next `execlaw service restart`.
    pub bind_address: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeneralSettingsUpdate {
    pub start_on_boot: Option<bool>,
    pub bind_address: Option<String>,
}

#[derive(Debug, Error)]
pub enum GeneralSettingsError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid bind address: {0}")]
    InvalidBindAddress(String),
}

pub struct GeneralSettingsStore<'a> {
    db: &'a Database,
}

impl<'a> GeneralSettingsStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Fetch the singleton row. Migration 0017 seeds it on first run
    /// so this should never return `None` in production; the
    /// `Result<Option<_>>` shape is defensive against migration drift.
    pub fn get(&self) -> Result<Option<GeneralSettings>, GeneralSettingsError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT start_on_boot, bind_address, updated_at \
                 FROM config_general WHERE id = 1",
            )?;
            let row = stmt
                .query_row([], |r| {
                    Ok(GeneralSettings {
                        start_on_boot: r.get::<_, i64>(0)? != 0,
                        bind_address: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                })
                .ok();
            Ok(row)
        })
        .map_err(GeneralSettingsError::from)
    }

    /// Apply an update. Validates the bind address before writing.
    /// `now` is the timestamp to record on `updated_at`; callers
    /// pass `chrono::Utc::now().timestamp()`.
    pub fn update(
        &self,
        upd: &GeneralSettingsUpdate,
        now: i64,
    ) -> Result<GeneralSettings, GeneralSettingsError> {
        if let Some(addr) = &upd.bind_address {
            validate_bind_address(addr)?;
        }
        let upd = upd.clone();
        let saved = self.db.with_conn(|c| {
            // Read-modify-write so a partial update preserves the
            // other field. Singleton row → no contention concerns
            // beyond the connection pool's serialization.
            let current: Option<(i64, String)> = c
                .query_row(
                    "SELECT start_on_boot, bind_address FROM config_general WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            let (cur_boot, cur_bind) =
                current.unwrap_or((1, "127.0.0.1:3030".to_owned()));
            let new_boot = upd.start_on_boot.map(|b| b as i64).unwrap_or(cur_boot);
            let new_bind = upd.bind_address.clone().unwrap_or(cur_bind);
            c.execute(
                "INSERT INTO config_general (id, start_on_boot, bind_address, updated_at) \
                 VALUES (1, ?1, ?2, ?3) \
                 ON CONFLICT(id) DO UPDATE SET \
                    start_on_boot = excluded.start_on_boot, \
                    bind_address = excluded.bind_address, \
                    updated_at = excluded.updated_at",
                params![new_boot, new_bind, now],
            )?;
            Ok(GeneralSettings {
                start_on_boot: new_boot != 0,
                bind_address: new_bind,
                updated_at: now,
            })
        })?;
        Ok(saved)
    }
}

/// Sanity-check a `host:port` string. We don't resolve DNS here —
/// `execlaw serve` does that when it actually binds, and the
/// service-manager registration path doesn't care. We just refuse
/// strings that obviously can't bind.
fn validate_bind_address(addr: &str) -> Result<(), GeneralSettingsError> {
    use std::net::ToSocketAddrs;
    if addr.trim().is_empty() {
        return Err(GeneralSettingsError::InvalidBindAddress(
            "bind address must not be empty".into(),
        ));
    }
    // Quick sanity: `parse::<SocketAddr>()` works for IPs but not
    // hostnames; `to_socket_addrs` covers both. Short timeout-free
    // parse.
    if addr.parse::<std::net::SocketAddr>().is_err() {
        // Not an IP literal — try DNS-resolution shape via the
        // sync iterator. We discard the result; we just want
        // syntactic acceptance.
        let resolved: Result<Vec<_>, _> = addr.to_socket_addrs().map(|i| i.collect());
        if resolved.is_err() {
            return Err(GeneralSettingsError::InvalidBindAddress(format!(
                "could not parse '{addr}' as host:port"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn open() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn get_returns_seeded_defaults() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        let s = store.get().unwrap().expect("seed must exist");
        assert!(s.start_on_boot);
        assert_eq!(s.bind_address, "127.0.0.1:3030");
    }

    #[test]
    fn update_partial_preserves_other_field() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store
            .update(
                &GeneralSettingsUpdate {
                    start_on_boot: Some(false),
                    bind_address: None,
                },
                100,
            )
            .unwrap();
        let s = store.get().unwrap().unwrap();
        assert!(!s.start_on_boot);
        assert_eq!(
            s.bind_address, "127.0.0.1:3030",
            "untouched field must keep its prior value"
        );
    }

    #[test]
    fn update_writes_bind_address_and_timestamp() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store
            .update(
                &GeneralSettingsUpdate {
                    start_on_boot: None,
                    bind_address: Some("0.0.0.0:7777".into()),
                },
                4242,
            )
            .unwrap();
        let s = store.get().unwrap().unwrap();
        assert_eq!(s.bind_address, "0.0.0.0:7777");
        assert_eq!(s.updated_at, 4242);
    }

    #[test]
    fn update_rejects_empty_bind_address() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        let r = store.update(
            &GeneralSettingsUpdate {
                start_on_boot: None,
                bind_address: Some("".into()),
            },
            100,
        );
        assert!(matches!(
            r,
            Err(GeneralSettingsError::InvalidBindAddress(_))
        ));
    }

    #[test]
    fn update_rejects_garbage_bind_address() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        let r = store.update(
            &GeneralSettingsUpdate {
                start_on_boot: None,
                bind_address: Some("not a host port".into()),
            },
            100,
        );
        assert!(matches!(
            r,
            Err(GeneralSettingsError::InvalidBindAddress(_))
        ));
    }

    #[test]
    fn update_accepts_ipv6_literal() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store
            .update(
                &GeneralSettingsUpdate {
                    start_on_boot: None,
                    bind_address: Some("[::1]:3030".into()),
                },
                100,
            )
            .unwrap();
    }

    #[test]
    fn singleton_row_invariant() {
        // The CHECK constraint should refuse any INSERT with id != 1.
        let db = open();
        let r = db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_general (id, start_on_boot, bind_address, updated_at) \
                 VALUES (2, 1, '0.0.0.0:80', unixepoch())",
                [],
            )?;
            Ok(())
        });
        assert!(r.is_err(), "singleton row CHECK must reject id != 1");
    }
}
