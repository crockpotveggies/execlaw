//! Long-term memory helpers (§2.7).
//!
//! Trust-class scoping is enforced at the tool-shim layer by the policy
//! engine (§7.3). This module just owns the DB shape.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub scope: String,       // e.g. "principal:<id>" or "global"
    pub trust_class: String, // Controller | KnownTrusted | ...
    pub key: String,
    pub value_blob: Vec<u8>, // MessagePack
    pub ttl_expires: Option<i64>,
    pub updated_at: i64,
}

pub struct MemoryStore<'db> {
    db: &'db Database,
}

impl<'db> MemoryStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn upsert(&self, entry: &MemoryEntry) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO memory_entries(scope, trust_class, key, value_blob, ttl_expires, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(scope, trust_class, key) DO UPDATE SET \
                     value_blob = excluded.value_blob, \
                     ttl_expires = excluded.ttl_expires, \
                     updated_at = excluded.updated_at",
                params![
                    entry.scope,
                    entry.trust_class,
                    entry.key,
                    entry.value_blob,
                    entry.ttl_expires,
                    entry.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get(
        &self,
        scope: &str,
        trust_class: &str,
        key: &str,
    ) -> Result<Option<MemoryEntry>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT value_blob, ttl_expires, updated_at FROM memory_entries \
                     WHERE scope = ?1 AND trust_class = ?2 AND key = ?3",
                    params![scope, trust_class, key],
                    |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, i64>(2)?)),
                )
                .ok();
            Ok(got.map(|(value_blob, ttl_expires, updated_at)| MemoryEntry {
                scope: scope.to_owned(),
                trust_class: trust_class.to_owned(),
                key: key.to_owned(),
                value_blob,
                ttl_expires,
                updated_at,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    #[test]
    fn upsert_and_get_roundtrip() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = MemoryStore::new(&db);
        let entry = MemoryEntry {
            scope: "global".into(),
            trust_class: "Controller".into(),
            key: "favorite_voice".into(),
            value_blob: b"bf_emma".to_vec(),
            ttl_expires: None,
            updated_at: 1,
        };
        store.upsert(&entry).unwrap();
        let got = store.get("global", "Controller", "favorite_voice").unwrap().unwrap();
        assert_eq!(got.value_blob, entry.value_blob);
    }

    #[test]
    fn trust_class_scoping_is_enforced_by_primary_key() {
        // Same scope/key under different trust_class rows coexist.
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = MemoryStore::new(&db);
        store
            .upsert(&MemoryEntry {
                scope: "s".into(),
                trust_class: "Controller".into(),
                key: "k".into(),
                value_blob: b"c".to_vec(),
                ttl_expires: None,
                updated_at: 1,
            })
            .unwrap();
        store
            .upsert(&MemoryEntry {
                scope: "s".into(),
                trust_class: "KnownTrusted".into(),
                key: "k".into(),
                value_blob: b"kt".to_vec(),
                ttl_expires: None,
                updated_at: 1,
            })
            .unwrap();

        let c = store.get("s", "Controller", "k").unwrap().unwrap();
        let kt = store.get("s", "KnownTrusted", "k").unwrap().unwrap();
        assert_eq!(c.value_blob, b"c");
        assert_eq!(kt.value_blob, b"kt");
    }
}
