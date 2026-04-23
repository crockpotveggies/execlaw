//! Transport-plugin cursors (§2.15). One row per plugin, storing where the
//! plugin last persisted its inbound-poll cursor.

use crate::db::{Database, DbError};
use rusqlite::params;

pub struct TransportCursorStore<'db> {
    db: &'db Database,
}

impl<'db> TransportCursorStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn set(&self, plugin_id: &str, cursor_value: &str, at: i64) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO transport_cursors(plugin_id, cursor_value, updated_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(plugin_id) DO UPDATE SET \
                    cursor_value = excluded.cursor_value, \
                    updated_at = excluded.updated_at",
                params![plugin_id, cursor_value, at],
            )?;
            Ok(())
        })
    }

    pub fn get(&self, plugin_id: &str) -> Result<Option<String>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT cursor_value FROM transport_cursors WHERE plugin_id = ?1",
                    params![plugin_id],
                    |r| r.get::<_, String>(0),
                )
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
    fn set_and_get_roundtrip() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = TransportCursorStore::new(&db);
        store.set("plugin-signal", "source-evt-123", 1).unwrap();
        assert_eq!(
            store.get("plugin-signal").unwrap().as_deref(),
            Some("source-evt-123")
        );
        store.set("plugin-signal", "source-evt-124", 2).unwrap();
        assert_eq!(
            store.get("plugin-signal").unwrap().as_deref(),
            Some("source-evt-124")
        );
    }
}
