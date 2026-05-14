//! Raw `vault_secrets` row access.
//!
//! Higher-level secret semantics (opaque references, plugin-scoping) live
//! in the `execlaw-vault` crate. This module just lets core insert +
//! retrieve byte blobs from the table.

use crate::db::{Database, DbError};
use rusqlite::params;

pub struct VaultRowStore<'db> {
    db: &'db Database,
}

impl<'db> VaultRowStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn put(
        &self,
        plugin_id: Option<&str>,
        name: &str,
        value: &[u8],
        at: i64,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO vault_secrets(name, plugin_id, value_blob, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4) \
                 ON CONFLICT(plugin_id, name) DO UPDATE SET \
                    value_blob = excluded.value_blob, \
                    updated_at = excluded.updated_at",
                params![name, plugin_id, value, at],
            )?;
            Ok(())
        })
    }

    /// Delete a single secret by `(plugin_id, name)`. Returns
    /// `true` when a row was removed, `false` when no such row
    /// existed. Idempotent — operators / plugins can call this
    /// without checking presence first.
    pub fn delete(&self, plugin_id: Option<&str>, name: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = match plugin_id {
                Some(pid) => c.execute(
                    "DELETE FROM vault_secrets WHERE plugin_id = ?1 AND name = ?2",
                    params![pid, name],
                )?,
                None => c.execute(
                    "DELETE FROM vault_secrets WHERE plugin_id IS NULL AND name = ?1",
                    params![name],
                )?,
            };
            Ok(n > 0)
        })
    }

    /// Delete every vault row owned by `plugin_id`. Used by the
    /// plugin-lifecycle `purge` path so an uninstall clears the
    /// plugin's stored credentials / API keys / cached session data
    /// rather than letting them outlive the install. Core-scope rows
    /// (`plugin_id IS NULL`) are never touched. Returns the number of
    /// rows removed; 0 is a fine, idempotent answer.
    pub fn delete_for_plugin(&self, plugin_id: &str) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM vault_secrets WHERE plugin_id = ?1",
                params![plugin_id],
            )?;
            Ok(n)
        })
    }

    pub fn get(&self, plugin_id: Option<&str>, name: &str) -> Result<Option<Vec<u8>>, DbError> {
        self.db.with_conn(|c| {
            let got = match plugin_id {
                Some(pid) => c
                    .query_row(
                        "SELECT value_blob FROM vault_secrets WHERE plugin_id = ?1 AND name = ?2",
                        params![pid, name],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .ok(),
                None => c
                    .query_row(
                        "SELECT value_blob FROM vault_secrets WHERE plugin_id IS NULL AND name = ?1",
                        params![name],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .ok(),
            };
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
    fn put_and_get_roundtrip_core_scope() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let s = VaultRowStore::new(&db);
        s.put(None, "admin_password_hash", b"argon2-thingy", 1)
            .unwrap();
        assert_eq!(
            s.get(None, "admin_password_hash").unwrap().as_deref(),
            Some(&b"argon2-thingy"[..])
        );
    }

    #[test]
    fn delete_for_plugin_wipes_only_that_plugin_and_preserves_core_scope() {
        // The plugin-lifecycle `purge` path uses this to clear a
        // plugin's stored credentials on uninstall. Core-scope rows
        // (`plugin_id IS NULL`) must survive — those belong to the
        // host (admin_password_hash, JWT signing key, event-log HMAC
        // key) and a plugin uninstall must never disturb them.
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let s = VaultRowStore::new(&db);
        // Two rows under "signal", one under "whatsapp", one
        // core-scope. After delete_for_plugin("signal") only the
        // last two survive.
        s.put(Some("signal"), "linked_device_token", b"sig-tok", 1)
            .unwrap();
        s.put(Some("signal"), "registration_lock_pin", b"sig-pin", 1)
            .unwrap();
        s.put(Some("whatsapp"), "session", b"wa-sess", 1).unwrap();
        s.put(None, "admin_password_hash", b"argon2-thingy", 1)
            .unwrap();

        let removed = s.delete_for_plugin("signal").unwrap();
        assert_eq!(removed, 2, "both signal rows must be removed");
        assert!(s.get(Some("signal"), "linked_device_token").unwrap().is_none());
        assert!(s.get(Some("signal"), "registration_lock_pin").unwrap().is_none());
        // Other plugin and core-scope rows must be untouched.
        assert_eq!(
            s.get(Some("whatsapp"), "session").unwrap().as_deref(),
            Some(&b"wa-sess"[..])
        );
        assert_eq!(
            s.get(None, "admin_password_hash").unwrap().as_deref(),
            Some(&b"argon2-thingy"[..]),
            "core-scope rows must never be touched by a plugin purge",
        );

        // Idempotent — second call removes 0.
        assert_eq!(s.delete_for_plugin("signal").unwrap(), 0);
    }

    #[test]
    fn plugin_scope_is_isolated_from_core_scope() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let s = VaultRowStore::new(&db);
        s.put(None, "token", b"core_token", 1).unwrap();
        s.put(Some("google-calendar"), "token", b"plugin_token", 1)
            .unwrap();
        assert_eq!(
            s.get(None, "token").unwrap().as_deref(),
            Some(&b"core_token"[..])
        );
        assert_eq!(
            s.get(Some("google-calendar"), "token").unwrap().as_deref(),
            Some(&b"plugin_token"[..])
        );
    }
}
