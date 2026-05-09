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
    /// `host:port` the service listens on. Default `127.0.0.1:3031`.
    /// Edit takes effect on next `execlaw service restart`.
    pub bind_address: String,
    pub updated_at: i64,
    /// Phase 14 — first-run wizard "skip for now" timestamp. `Some`
    /// when the operator dismissed the backend step explicitly;
    /// `None` until they do or until they configure a backend (in
    /// which case the wizard naturally completes through the
    /// Standard-backend-exists path). The `/api/ping` handler reads
    /// this together with `config_backends` to decide between
    /// `wizard` and `pong`.
    pub setup_wizard_dismissed_at: Option<i64>,
    /// 2026-04-29 — global history-retention window in days. `0`
    /// means infinite (never delete). Other legal values are 30 /
    /// 60 / 90 / 120 (see `crate::retention::ALLOWED_RETENTION_DAYS`).
    /// Sweepers consume via [`crate::retention::RetentionPolicy::load`].
    pub history_retention_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeneralSettingsUpdate {
    pub start_on_boot: Option<bool>,
    pub bind_address: Option<String>,
    /// Phase 14 — `Some(true)` flips `setup_wizard_dismissed_at` to
    /// the current timestamp; `Some(false)` clears it (re-arms the
    /// wizard for testing). `None` leaves the column alone.
    pub setup_wizard_dismissed: Option<bool>,
    /// 2026-04-29 — operator's choice from the Settings → General
    /// retention dropdown. `Some(0)` = infinite; `Some(30/60/90/120)`
    /// = finite window; `None` leaves the column alone. Values
    /// outside that set are rejected by the API layer (validate
    /// before calling [`GeneralSettingsStore::update`]).
    pub history_retention_days: Option<u32>,
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
        self.db
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT start_on_boot, bind_address, updated_at, \
                        setup_wizard_dismissed_at, history_retention_days \
                 FROM config_general WHERE id = 1",
                )?;
                let row = stmt
                    .query_row([], |r| {
                        let raw_retention: i64 = r.get(4)?;
                        Ok(GeneralSettings {
                            start_on_boot: r.get::<_, i64>(0)? != 0,
                            bind_address: r.get(1)?,
                            updated_at: r.get(2)?,
                            setup_wizard_dismissed_at: r.get(3)?,
                            history_retention_days: raw_retention.max(0) as u32,
                        })
                    })
                    .ok();
                Ok(row)
            })
            .map_err(GeneralSettingsError::from)
    }

    /// Cheap "is the first-run wizard considered complete?" probe.
    /// Used by `/api/ping` so the SPA's AppBoot + setup guard can
    /// route the operator back to /setup until either condition
    /// holds. Mirrors the contract the wizard's "Skip for now"
    /// button writes to via [`Self::dismiss_setup_wizard`].
    pub fn wizard_dismissed(&self) -> Result<bool, GeneralSettingsError> {
        Ok(self
            .get()?
            .and_then(|s| s.setup_wizard_dismissed_at)
            .is_some())
    }

    /// Phase 14.C — read the operator-configured list of additional
    /// HuggingFace cache directories. The supervisor scans these
    /// (read-only) when the host-side downloader needs a model;
    /// already-downloaded files get hardlinked / copied into the
    /// primary cache, avoiding re-download.
    ///
    /// Stored as a JSON array of paths in `config_general`. We
    /// store as JSON rather than as a new table because the list is
    /// always small (< 10 entries) and we avoid joining on every
    /// supervisor reconcile. Returns an empty Vec when the column
    /// is null or unparseable.
    pub fn read_secondary_hf_caches(
        &self,
    ) -> Result<Vec<std::path::PathBuf>, GeneralSettingsError> {
        let raw: Option<String> = self.db.with_conn(|c| {
            let v: Option<String> = c
                .query_row(
                    "SELECT hf_secondary_caches_json FROM config_general WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            Ok(v)
        })?;
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        match serde_json::from_str::<Vec<String>>(&raw) {
            Ok(v) => Ok(v.into_iter().map(std::path::PathBuf::from).collect()),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Phase 14.C — write the operator-configured list of secondary
    /// HF caches. Settings UI calls this. Validates that every entry
    /// is non-empty + an absolute path; relative paths are rejected
    /// because the supervisor's reconcile loop runs from a service
    /// working directory the operator probably didn't expect.
    pub fn write_secondary_hf_caches(
        &self,
        paths: &[std::path::PathBuf],
        now: i64,
    ) -> Result<(), GeneralSettingsError> {
        for p in paths {
            if p.as_os_str().is_empty() {
                return Err(GeneralSettingsError::InvalidBindAddress(
                    "HF cache path must not be empty".into(),
                ));
            }
            // Cross-platform absolute-path check: `Path::is_absolute()`
            // is OS-specific (rejects `/home/me/...` on Windows, rejects
            // `C:\...` on Linux). Since this validation runs on the
            // server but the path string was minted by an operator
            // who might be configuring a remote server from any host,
            // we accept either shape: a leading `/` (POSIX absolute)
            // OR a `<drive>:` prefix followed by `\` or `/` (Windows
            // absolute). Relative paths still get rejected.
            let s = p.to_string_lossy();
            let posix_abs = s.starts_with('/');
            let windows_abs = s.len() >= 3
                && s.as_bytes()[1] == b':'
                && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/')
                && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
            if !posix_abs && !windows_abs {
                return Err(GeneralSettingsError::InvalidBindAddress(format!(
                    "HF cache path must be absolute: {}",
                    p.display()
                )));
            }
        }
        let payload = serde_json::to_string(
            &paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".to_owned());
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET hf_secondary_caches_json = ?1, updated_at = ?2 WHERE id = 1",
                params![payload, now],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Mark the wizard as dismissed at `now`. Idempotent — calling
    /// twice just rewrites the timestamp. The wizard's "Skip for
    /// now" button calls this immediately before navigating to
    /// /chat.
    pub fn dismiss_setup_wizard(&self, now: i64) -> Result<GeneralSettings, GeneralSettingsError> {
        self.update(
            &GeneralSettingsUpdate {
                setup_wizard_dismissed: Some(true),
                ..GeneralSettingsUpdate::default()
            },
            now,
        )
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
        if let Some(retention) = upd.history_retention_days {
            validate_retention_days(retention)?;
        }
        let upd = upd.clone();
        let saved = self.db.with_conn(|c| {
            // Read-modify-write so a partial update preserves
            // siblings. Singleton row → no contention concerns
            // beyond the connection pool's serialization.
            let current: Option<(i64, String, Option<i64>, i64)> = c
                .query_row(
                    "SELECT start_on_boot, bind_address, setup_wizard_dismissed_at, \
                            history_retention_days \
                     FROM config_general WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .ok();
            let (cur_boot, cur_bind, cur_dismissed, cur_retention) = current.unwrap_or((
                1,
                "127.0.0.1:3031".to_owned(),
                None,
                crate::retention::DEFAULT_RETENTION_DAYS as i64,
            ));
            let new_boot = upd.start_on_boot.map(|b| b as i64).unwrap_or(cur_boot);
            let new_bind = upd.bind_address.clone().unwrap_or(cur_bind);
            let new_dismissed: Option<i64> = match upd.setup_wizard_dismissed {
                Some(true) => Some(now),
                Some(false) => None,
                None => cur_dismissed,
            };
            let new_retention: i64 = upd
                .history_retention_days
                .map(|d| d as i64)
                .unwrap_or(cur_retention);
            c.execute(
                "INSERT INTO config_general \
                    (id, start_on_boot, bind_address, updated_at, setup_wizard_dismissed_at, \
                     history_retention_days) \
                 VALUES (1, ?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                    start_on_boot = excluded.start_on_boot, \
                    bind_address = excluded.bind_address, \
                    updated_at = excluded.updated_at, \
                    setup_wizard_dismissed_at = excluded.setup_wizard_dismissed_at, \
                    history_retention_days = excluded.history_retention_days",
                params![new_boot, new_bind, now, new_dismissed, new_retention],
            )?;
            Ok(GeneralSettings {
                start_on_boot: new_boot != 0,
                bind_address: new_bind,
                updated_at: now,
                setup_wizard_dismissed_at: new_dismissed,
                history_retention_days: new_retention.max(0) as u32,
            })
        })?;
        Ok(saved)
    }
}

/// Reject retention-days values that aren't `0` (infinite) or one of
/// the legal `ALLOWED_RETENTION_DAYS` choices. The Settings UI
/// dropdown enforces this client-side; the server enforces it again
/// so a malformed API call can't slip a 45 or a 1000 into the column.
fn validate_retention_days(days: u32) -> Result<(), GeneralSettingsError> {
    if days == crate::retention::INFINITE_RETENTION
        || crate::retention::ALLOWED_RETENTION_DAYS.contains(&days)
    {
        return Ok(());
    }
    Err(GeneralSettingsError::InvalidBindAddress(format!(
        "history_retention_days={days} is not a legal option; \
         expected 0 (infinite) or one of {:?}",
        crate::retention::ALLOWED_RETENTION_DAYS
    )))
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
        assert_eq!(s.bind_address, "127.0.0.1:3031");
    }

    #[test]
    fn update_partial_preserves_other_field() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store
            .update(
                &GeneralSettingsUpdate {
                    start_on_boot: Some(false),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        let s = store.get().unwrap().unwrap();
        assert!(!s.start_on_boot);
        assert_eq!(
            s.bind_address, "127.0.0.1:3031",
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
                    bind_address: Some("0.0.0.0:7777".into()),
                    ..Default::default()
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
                bind_address: Some("".into()),
                ..Default::default()
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
                bind_address: Some("not a host port".into()),
                ..Default::default()
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
                    bind_address: Some("[::1]:3031".into()),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
    }

    #[test]
    fn dismiss_setup_wizard_records_timestamp_and_round_trips() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        assert!(!store.wizard_dismissed().unwrap());
        store.dismiss_setup_wizard(9999).unwrap();
        assert!(store.wizard_dismissed().unwrap());
        let s = store.get().unwrap().unwrap();
        assert_eq!(s.setup_wizard_dismissed_at, Some(9999));
    }

    #[test]
    fn fresh_db_seeds_default_history_retention() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        let s = store.get().unwrap().unwrap();
        assert_eq!(s.history_retention_days, 30);
    }

    #[test]
    fn update_writes_history_retention_days() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store
            .update(
                &GeneralSettingsUpdate {
                    history_retention_days: Some(90),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        let s = store.get().unwrap().unwrap();
        assert_eq!(s.history_retention_days, 90);
    }

    #[test]
    fn update_accepts_infinite_retention_zero() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store
            .update(
                &GeneralSettingsUpdate {
                    history_retention_days: Some(0),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        let s = store.get().unwrap().unwrap();
        assert_eq!(s.history_retention_days, 0);
    }

    /// API-level guard: writes outside the legal option set are
    /// rejected even though the column would technically accept any
    /// integer. Defense against a malformed Settings save.
    #[test]
    fn update_rejects_illegal_retention_value() {
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        let r = store.update(
            &GeneralSettingsUpdate {
                history_retention_days: Some(45),
                ..Default::default()
            },
            100,
        );
        assert!(matches!(
            r,
            Err(GeneralSettingsError::InvalidBindAddress(_))
        ));
    }

    #[test]
    fn dismiss_then_clear_resets_wizard_state() {
        // Test re-arming the wizard via the explicit `Some(false)`
        // branch in GeneralSettingsUpdate.
        let db = open();
        let store = GeneralSettingsStore::new(&db);
        store.dismiss_setup_wizard(100).unwrap();
        assert!(store.wizard_dismissed().unwrap());
        store
            .update(
                &GeneralSettingsUpdate {
                    setup_wizard_dismissed: Some(false),
                    ..Default::default()
                },
                200,
            )
            .unwrap();
        assert!(!store.wizard_dismissed().unwrap());
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
