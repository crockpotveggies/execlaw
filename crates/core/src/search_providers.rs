//! `config_search_providers` row + CRUD store.
//!
//! Backs the Settings → Search page (provider picker + per-kind
//! config) and the dispatch-time provider resolver. The deep-
//! research gather phase + the agent's `web_search` tool both look
//! up the active provider here at dispatch time, so swapping
//! providers takes effect on the next turn — no server restart.
//!
//! Per-kind config_json shapes:
//!   * `duckduckgo` — `{}` (no config)
//!   * `searxng` — `{"base_url": "https://searx.example.com"}`
//!   * `brave` — `{"api_key": "..."}`
//!   * `tavily` — `{"api_key": "..."}`
//!
//! The store doesn't validate per-kind config shapes — that's the
//! adapter's job at construction time. This keeps `core` provider-
//! agnostic and lets new adapters land without core-crate churn.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Closed enum of provider kinds the host knows about. Stays in
/// `core` so the dispatcher + admin endpoints can match on it
/// without depending on `server`. New adapters add a variant here
/// + an `as_str` / `parse` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProviderKind {
    /// HTML scrape of `html.duckduckgo.com`. No config. Default
    /// on first boot. Aggressive bot detection on busy sessions —
    /// operators with rate-limit bounce-backs should switch.
    DuckDuckGo,
    /// Self-hosted SearxNG meta-search. Operator runs a SearxNG
    /// container locally; this adapter POSTs `format=json&q=...`
    /// against the configured base URL. No API key, no shared
    /// rate limit — aligns with execlaw's grounding rule.
    SearxNG,
    /// Brave Search API. Paid (~$5 per 1k queries) but extremely
    /// fast + reliable + AI-research-friendly. Requires an API
    /// key from search.brave.com.
    Brave,
}

impl SearchProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::SearxNG => "searxng",
            Self::Brave => "brave",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "duckduckgo" => Some(Self::DuckDuckGo),
            "searxng" => Some(Self::SearxNG),
            "brave" => Some(Self::Brave),
            _ => None,
        }
    }

    /// Operator-facing label for the Settings UI. The `as_str`
    /// value is the wire identifier; this is the human-readable
    /// name. Kept on the kind itself so a SPA-side label lookup
    /// can't drift from the canonical list.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "DuckDuckGo",
            Self::SearxNG => "SearxNG (self-hosted)",
            Self::Brave => "Brave Search API",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchProviderRow {
    pub kind: SearchProviderKind,
    pub enabled: bool,
    pub is_default: bool,
    /// Per-kind JSON. See module-level doc for shapes.
    pub config_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Error)]
pub enum SearchProviderError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("unknown provider kind: {0}")]
    UnknownKind(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

pub struct SearchProviderStore<'db> {
    db: &'db Database,
}

impl<'db> SearchProviderStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert or replace a provider row. Use this when the operator
    /// adds a new provider OR updates an existing one's config.
    /// Idempotent on `kind` (PRIMARY KEY).
    pub fn upsert(&self, row: &SearchProviderRow) -> Result<(), SearchProviderError> {
        let kind = row.kind.as_str().to_owned();
        let enabled = if row.enabled { 1 } else { 0 };
        let is_default = if row.is_default { 1 } else { 0 };
        let config = row.config_json.clone();
        let created = row.created_at;
        let updated = row.updated_at;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_search_providers
                    (kind, enabled, is_default, config_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(kind) DO UPDATE SET
                    enabled     = excluded.enabled,
                    is_default  = excluded.is_default,
                    config_json = excluded.config_json,
                    updated_at  = excluded.updated_at",
                params![kind, enabled, is_default, config, created, updated],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Look up a single row by kind. `None` when the kind has no
    /// row in the table.
    pub fn get(&self, kind: SearchProviderKind) -> Result<Option<SearchProviderRow>, SearchProviderError> {
        let kind_str = kind.as_str().to_owned();
        let row = self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT kind, enabled, is_default, config_json, created_at, updated_at
                     FROM config_search_providers WHERE kind = ?1",
                    params![kind_str],
                    row_from_sqlite,
                )
                .ok();
            Ok(row)
        })?;
        Ok(row.transpose()?)
    }

    /// List every row, sorted by `kind` for stable ordering. The
    /// SPA renders these in a list so the operator can pick which
    /// is active.
    pub fn list_all(&self) -> Result<Vec<SearchProviderRow>, SearchProviderError> {
        let rows: Vec<Result<SearchProviderRow, SearchProviderError>> = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, enabled, is_default, config_json, created_at, updated_at
                 FROM config_search_providers ORDER BY kind ASC",
            )?;
            let rows = stmt
                .query_map([], row_from_sqlite)?
                .map(|r| match r {
                    Ok(inner) => inner,
                    Err(e) => Err(SearchProviderError::Db(DbError::Sqlite(e))),
                })
                .collect::<Vec<_>>();
            Ok(rows)
        })?;
        rows.into_iter().collect()
    }

    /// Resolve the active provider for dispatch. Returns the row
    /// where `enabled = 1 AND is_default = 1` (the trigger
    /// guarantees at most one such row). `None` when no provider
    /// is active — caller should fall back to a hard-coded default
    /// or refuse to dispatch.
    pub fn active(&self) -> Result<Option<SearchProviderRow>, SearchProviderError> {
        let row = self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT kind, enabled, is_default, config_json, created_at, updated_at
                     FROM config_search_providers
                     WHERE enabled = 1 AND is_default = 1
                     LIMIT 1",
                    [],
                    row_from_sqlite,
                )
                .ok();
            Ok(row)
        })?;
        Ok(row.transpose()?)
    }

    /// Mark a provider as the default. Atomic — the trigger flips
    /// `is_default` off on every other row in the same statement.
    /// Returns `true` when the row transitioned, `false` when no
    /// such row exists (caller can decide whether to upsert first).
    pub fn set_default(&self, kind: SearchProviderKind, now: i64) -> Result<bool, SearchProviderError> {
        let kind_str = kind.as_str().to_owned();
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE config_search_providers
                 SET is_default = 1, updated_at = ?1
                 WHERE kind = ?2",
                params![now, kind_str],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Delete a provider row entirely. The default-provider
    /// trigger doesn't fire on DELETE, so if the deleted row was
    /// the default, the system is left with no default. Caller is
    /// responsible for picking a new one (the SPA prevents this
    /// at the UI layer).
    pub fn delete(&self, kind: SearchProviderKind) -> Result<bool, SearchProviderError> {
        let kind_str = kind.as_str().to_owned();
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM config_search_providers WHERE kind = ?1",
                params![kind_str],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

fn row_from_sqlite(r: &rusqlite::Row) -> rusqlite::Result<Result<SearchProviderRow, SearchProviderError>> {
    let kind_str: String = r.get(0)?;
    let kind = match SearchProviderKind::parse(&kind_str) {
        Some(k) => k,
        None => return Ok(Err(SearchProviderError::UnknownKind(kind_str))),
    };
    Ok(Ok(SearchProviderRow {
        kind,
        enabled: r.get::<_, i64>(1)? != 0,
        is_default: r.get::<_, i64>(2)? != 0,
        config_json: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    }))
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

    #[test]
    fn migration_seeds_duckduckgo_as_default() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        let active = store.active().unwrap().unwrap();
        assert_eq!(active.kind, SearchProviderKind::DuckDuckGo);
        assert!(active.enabled);
        assert!(active.is_default);
    }

    #[test]
    fn upsert_inserts_then_updates_on_same_kind() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::SearxNG,
                enabled: true,
                is_default: false,
                config_json: r#"{"base_url":"https://a.example.com"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let r1 = store.get(SearchProviderKind::SearxNG).unwrap().unwrap();
        assert!(r1.config_json.contains("a.example.com"));

        // Update — same kind, new config.
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::SearxNG,
                enabled: true,
                is_default: false,
                config_json: r#"{"base_url":"https://b.example.com"}"#.into(),
                created_at: 100,
                updated_at: 200,
            })
            .unwrap();
        let r2 = store.get(SearchProviderKind::SearxNG).unwrap().unwrap();
        assert!(r2.config_json.contains("b.example.com"));
        assert_eq!(r2.updated_at, 200);
    }

    #[test]
    fn set_default_clears_other_defaults_via_trigger() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        // DDG is the seeded default. Add SearxNG and promote it.
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::SearxNG,
                enabled: true,
                is_default: false,
                config_json: r#"{"base_url":"https://x.example.com"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let promoted = store.set_default(SearchProviderKind::SearxNG, 300).unwrap();
        assert!(promoted);

        // SearxNG is now default, DDG is not.
        let active = store.active().unwrap().unwrap();
        assert_eq!(active.kind, SearchProviderKind::SearxNG);
        let ddg = store.get(SearchProviderKind::DuckDuckGo).unwrap().unwrap();
        assert!(!ddg.is_default);

        // Sanity: only one row has is_default=1.
        let all = store.list_all().unwrap();
        let defaults = all.iter().filter(|r| r.is_default).count();
        assert_eq!(defaults, 1);
    }

    #[test]
    fn list_all_returns_sorted_by_kind_alphabetically() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::SearxNG,
                enabled: true,
                is_default: false,
                config_json: "{}".into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Brave,
                enabled: true,
                is_default: false,
                config_json: "{}".into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let rows = store.list_all().unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["brave", "duckduckgo", "searxng"]);
    }

    #[test]
    fn delete_removes_row_but_does_not_repromote_default() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Brave,
                enabled: true,
                is_default: false,
                config_json: r#"{"api_key":"x"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let removed = store.delete(SearchProviderKind::Brave).unwrap();
        assert!(removed);
        assert!(store.get(SearchProviderKind::Brave).unwrap().is_none());
        // DDG still default.
        assert_eq!(
            store.active().unwrap().unwrap().kind,
            SearchProviderKind::DuckDuckGo,
        );
    }

    #[test]
    fn parse_round_trips_every_kind() {
        for k in [
            SearchProviderKind::DuckDuckGo,
            SearchProviderKind::SearxNG,
            SearchProviderKind::Brave,
        ] {
            assert_eq!(SearchProviderKind::parse(k.as_str()), Some(k));
            assert!(!k.display_name().is_empty());
        }
    }
}
