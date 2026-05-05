//! `eval_flagged` row + store (Phase 5 observability).
//!
//! Operators tag event ranges as regression targets via the
//! `execlaw eval flag` CLI. The LLM-judge harness reads matching
//! rows by label to decide which traces to replay against rubrics.

use crate::db::{Database, DbError};
use crate::ids::ConversationId;
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalFlagRow {
    /// `None` until `insert` returns the auto-incremented row id.
    pub id: Option<i64>,
    pub conversation_id: ConversationId,
    /// Inclusive lower bound of the flagged event range.
    pub from_seq: i64,
    /// Inclusive upper bound.
    pub to_seq: i64,
    pub label: String,
    /// Free-form tags carried as JSON; the LLM-judge can pick rubrics
    /// based on these (e.g. `["trust-class", "rule-of-two"]`).
    pub tags: Vec<String>,
    /// Principal id of the operator who flagged this range.
    pub flagged_by: String,
    /// Unix-seconds timestamp.
    pub flagged_at: i64,
    pub notes: Option<String>,
}

pub struct EvalFlaggedStore<'db> {
    db: &'db Database,
}

impl<'db> EvalFlaggedStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a flag. Returns the new row's auto-incremented id.
    pub fn insert(&self, row: &EvalFlagRow) -> Result<i64, DbError> {
        if row.from_seq > row.to_seq {
            return Err(DbError::Invariant(format!(
                "from_seq ({}) must be <= to_seq ({})",
                row.from_seq, row.to_seq
            )));
        }
        let tags_json =
            serde_json::to_vec(&row.tags).map_err(|e| DbError::Serde(format!("tags: {e}")))?;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO eval_flagged \
                 (conversation_id, from_seq, to_seq, label, tags_json, flagged_by, flagged_at, notes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.conversation_id.as_str(),
                    row.from_seq,
                    row.to_seq,
                    row.label,
                    tags_json,
                    row.flagged_by,
                    row.flagged_at,
                    row.notes,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// List every flag, newest first.
    pub fn list_all(&self) -> Result<Vec<EvalFlagRow>, DbError> {
        self.query_all(None)
    }

    /// List every flag with the given label, newest first.
    pub fn list_by_label(&self, label: &str) -> Result<Vec<EvalFlagRow>, DbError> {
        self.query_all(Some(label))
    }

    fn query_all(&self, label: Option<&str>) -> Result<Vec<EvalFlagRow>, DbError> {
        self.db.with_conn(|c| {
            let (sql, label_param): (&str, Option<&str>) = match label {
                Some(_) => (
                    "SELECT id, conversation_id, from_seq, to_seq, label, tags_json, \
                            flagged_by, flagged_at, notes \
                     FROM eval_flagged WHERE label = ?1 ORDER BY flagged_at DESC",
                    label,
                ),
                None => (
                    "SELECT id, conversation_id, from_seq, to_seq, label, tags_json, \
                            flagged_by, flagged_at, notes \
                     FROM eval_flagged ORDER BY flagged_at DESC",
                    None,
                ),
            };
            let mut stmt = c.prepare_cached(sql)?;
            let rows = match label_param {
                Some(l) => stmt
                    .query_map(params![l], row_to_flag)?
                    .collect::<Result<Vec<_>, _>>()?,
                None => stmt
                    .query_map([], row_to_flag)?
                    .collect::<Result<Vec<_>, _>>()?,
            };
            let mut out = Vec::with_capacity(rows.len());
            for (row, tags_json) in rows {
                let tags: Vec<String> = serde_json::from_slice(&tags_json)
                    .map_err(|e| DbError::Serde(format!("tags: {e}")))?;
                out.push(EvalFlagRow { tags, ..row });
            }
            Ok(out)
        })
    }
}

#[allow(clippy::type_complexity)]
fn row_to_flag(r: &rusqlite::Row<'_>) -> rusqlite::Result<(EvalFlagRow, Vec<u8>)> {
    let id: i64 = r.get(0)?;
    let conv: String = r.get(1)?;
    let from_seq: i64 = r.get(2)?;
    let to_seq: i64 = r.get(3)?;
    let label: String = r.get(4)?;
    let tags_json: Vec<u8> = r
        .get::<_, Option<Vec<u8>>>(5)?
        .unwrap_or_else(|| b"[]".to_vec());
    let flagged_by: String = r.get(6)?;
    let flagged_at: i64 = r.get(7)?;
    let notes: Option<String> = r.get(8)?;
    Ok((
        EvalFlagRow {
            id: Some(id),
            conversation_id: ConversationId::from(conv),
            from_seq,
            to_seq,
            label,
            tags: vec![], // re-decoded by caller
            flagged_by,
            flagged_at,
            notes,
        },
        tags_json,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn mk_row(label: &str) -> EvalFlagRow {
        EvalFlagRow {
            id: None,
            conversation_id: ConversationId::from("c1"),
            from_seq: 1,
            to_seq: 10,
            label: label.into(),
            tags: vec!["trust-class".into()],
            flagged_by: "controller".into(),
            flagged_at: 100,
            notes: Some("the model leaked the api_key".into()),
        }
    }

    #[test]
    fn insert_and_list_round_trips_full_row() {
        let db = fresh_db();
        let store = EvalFlaggedStore::new(&db);
        let id = store.insert(&mk_row("regression-1")).unwrap();
        assert!(id > 0);
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 1);
        let r = &all[0];
        assert_eq!(r.id, Some(id));
        assert_eq!(r.label, "regression-1");
        assert_eq!(r.tags, vec!["trust-class".to_string()]);
        assert_eq!(r.notes.as_deref(), Some("the model leaked the api_key"));
    }

    #[test]
    fn list_by_label_filters_correctly() {
        let db = fresh_db();
        let store = EvalFlaggedStore::new(&db);
        store.insert(&mk_row("alpha")).unwrap();
        store.insert(&mk_row("beta")).unwrap();
        store.insert(&mk_row("alpha")).unwrap();

        assert_eq!(store.list_by_label("alpha").unwrap().len(), 2);
        assert_eq!(store.list_by_label("beta").unwrap().len(), 1);
        assert_eq!(store.list_by_label("nonexistent").unwrap().len(), 0);
    }

    #[test]
    fn list_orders_newest_first() {
        let db = fresh_db();
        let store = EvalFlaggedStore::new(&db);
        let mut early = mk_row("x");
        early.flagged_at = 100;
        let mut late = mk_row("x");
        late.flagged_at = 200;
        // Insert out of order.
        store.insert(&early).unwrap();
        store.insert(&late).unwrap();
        let rows = store.list_all().unwrap();
        assert_eq!(rows[0].flagged_at, 200);
        assert_eq!(rows[1].flagged_at, 100);
    }

    /// Adversarial: from_seq > to_seq is rejected as an Invariant
    /// error. Operators with typos shouldn't be able to write a
    /// nonsensical range.
    #[test]
    fn inverted_range_is_rejected() {
        let db = fresh_db();
        let store = EvalFlaggedStore::new(&db);
        let mut row = mk_row("bad");
        row.from_seq = 50;
        row.to_seq = 10;
        let err = store.insert(&row).unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    #[test]
    fn empty_tags_round_trip() {
        let db = fresh_db();
        let store = EvalFlaggedStore::new(&db);
        let mut row = mk_row("notags");
        row.tags.clear();
        store.insert(&row).unwrap();
        let got = store.list_by_label("notags").unwrap();
        assert!(got[0].tags.is_empty());
    }
}
