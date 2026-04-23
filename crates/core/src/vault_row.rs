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
