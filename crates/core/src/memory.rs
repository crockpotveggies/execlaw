//! Long-term memory helpers (§2.7) + lifecycle (migration 0035).
//!
//! Trust-class scoping is enforced at the tool-shim layer by the policy
//! engine (§7.3). This module owns the DB shape, the per-row counters
//! (`hits`, `last_used_at`), and the tier query helpers used by the
//! runner's HOT-injection slot and by the promotion sweeper.
//!
//! Tier semantics:
//!   * `hot`  — included automatically in the per-turn system prompt
//!              by the runner's HOT slot. Promotion into HOT requires
//!              an approved `memory_promotions` row — agents cannot
//!              self-promote.
//!   * `warm` — readable on demand via `read_memory`. The default
//!              tier for every fresh write.
//!   * `cold` — excluded from default reads. Surface only via an
//!              explicit point lookup. Audit-friendly archive.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// The three lifecycle tiers a memory row can occupy. Persisted as
/// the lower-case string literals so SQL `WHERE tier = 'hot'` filters
/// stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTier {
    Hot,
    Warm,
    Cold,
}

impl MemoryTier {
    pub fn as_sql(self) -> &'static str {
        match self {
            MemoryTier::Hot => "hot",
            MemoryTier::Warm => "warm",
            MemoryTier::Cold => "cold",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hot" => Some(MemoryTier::Hot),
            "warm" => Some(MemoryTier::Warm),
            "cold" => Some(MemoryTier::Cold),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub scope: String,       // e.g. "principal:<id>" or "global"
    pub trust_class: String, // Controller | KnownTrusted | ...
    pub key: String,
    pub value_blob: Vec<u8>, // MessagePack
    pub ttl_expires: Option<i64>,
    pub updated_at: i64,
    /// Defaults to `Warm` for both fresh writes and pre-migration
    /// rows (the migration backfills the column with `'warm'`).
    #[serde(default = "default_tier")]
    pub tier: MemoryTier,
    /// Number of times `read_memory` returned this row. Drives the
    /// promotion sweeper.
    #[serde(default)]
    pub hits: u64,
    /// Unix seconds of the most-recent successful read. `None` until
    /// the row is read for the first time post-migration.
    #[serde(default)]
    pub last_used_at: Option<i64>,
    /// Unix seconds of first insert. Pre-migration rows are
    /// backfilled from `updated_at` (see migration 0035).
    #[serde(default)]
    pub created_at: i64,
}

fn default_tier() -> MemoryTier {
    MemoryTier::Warm
}

/// Lightweight projection used by `list_memory` / HOT-slot lookups
/// where callers don't need the full value blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRowSummary {
    pub scope: String,
    pub trust_class: String,
    pub key: String,
    pub tier: MemoryTier,
    pub hits: u64,
    pub last_used_at: Option<i64>,
    pub updated_at: i64,
}

pub struct MemoryStore<'db> {
    db: &'db Database,
}

impl<'db> MemoryStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert-or-update. The lifecycle columns are *not* clobbered on
    /// conflict — re-writing the value of a HOT entry must keep its
    /// HOT tier and accumulated hit count. Callers wanting to reset
    /// the lifecycle (e.g. after archive) should `delete` first.
    pub fn upsert(&self, entry: &MemoryEntry) -> Result<(), DbError> {
        let tier = entry.tier.as_sql();
        let created_at = if entry.created_at == 0 {
            entry.updated_at
        } else {
            entry.created_at
        };
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO memory_entries(\
                     scope, trust_class, key, value_blob, ttl_expires, updated_at, \
                     tier, hits, last_used_at, created_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(scope, trust_class, key) DO UPDATE SET \
                     value_blob   = excluded.value_blob, \
                     ttl_expires  = excluded.ttl_expires, \
                     updated_at   = excluded.updated_at",
                params![
                    entry.scope,
                    entry.trust_class,
                    entry.key,
                    entry.value_blob,
                    entry.ttl_expires,
                    entry.updated_at,
                    tier,
                    entry.hits as i64,
                    entry.last_used_at,
                    created_at,
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
                    "SELECT value_blob, ttl_expires, updated_at, \
                            tier, hits, last_used_at, created_at \
                     FROM memory_entries \
                     WHERE scope = ?1 AND trust_class = ?2 AND key = ?3",
                    params![scope, trust_class, key],
                    |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    },
                )
                .ok();
            Ok(got.map(
                |(value_blob, ttl_expires, updated_at, tier, hits, last_used_at, created_at)| {
                    MemoryEntry {
                        scope: scope.to_owned(),
                        trust_class: trust_class.to_owned(),
                        key: key.to_owned(),
                        value_blob,
                        ttl_expires,
                        updated_at,
                        tier: MemoryTier::parse(&tier).unwrap_or(MemoryTier::Warm),
                        hits: hits.max(0) as u64,
                        last_used_at,
                        created_at,
                    }
                },
            ))
        })
    }

    /// Atomically increment `hits` and stamp `last_used_at` on a
    /// row that was just successfully read. Callers should invoke
    /// this from the read-path tool shim AFTER a successful return,
    /// not on a miss. Returns the new hit count, or 0 if the row
    /// disappeared between read and bump (unusual, but possible
    /// under retention sweeper races).
    pub fn bump_hit(
        &self,
        scope: &str,
        trust_class: &str,
        key: &str,
        now_unix: i64,
    ) -> Result<u64, DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE memory_entries \
                    SET hits = hits + 1, last_used_at = ?4 \
                  WHERE scope = ?1 AND trust_class = ?2 AND key = ?3",
                params![scope, trust_class, key, now_unix],
            )?;
            let n: i64 = c
                .query_row(
                    "SELECT hits FROM memory_entries \
                     WHERE scope = ?1 AND trust_class = ?2 AND key = ?3",
                    params![scope, trust_class, key],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            Ok(n.max(0) as u64)
        })
    }

    /// All HOT-tier rows visible at one of the supplied trust classes
    /// for the given scope, ordered by `last_used_at DESC` (most
    /// recently useful first). The runner's HOT-injection slot calls
    /// this once per turn and caps the total byte budget itself —
    /// this query just returns the candidate set.
    ///
    /// `trust_classes` is the read-down chain for the caller, same
    /// shape `MemoryApi::read` uses.
    pub fn list_hot(
        &self,
        scope: &str,
        trust_classes: &[&str],
        limit: u32,
    ) -> Result<Vec<MemoryEntry>, DbError> {
        if trust_classes.is_empty() {
            return Ok(Vec::new());
        }
        self.db.with_conn(|c| {
            let placeholders = (0..trust_classes.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT scope, trust_class, key, value_blob, ttl_expires, updated_at, \
                        tier, hits, last_used_at, created_at \
                 FROM memory_entries \
                 WHERE scope = ?1 AND tier = 'hot' AND trust_class IN ({}) \
                 ORDER BY COALESCE(last_used_at, updated_at) DESC \
                 LIMIT ?{}",
                placeholders,
                trust_classes.len() + 2
            );
            let mut stmt = c.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(scope.to_owned())];
            for cls in trust_classes {
                params_vec.push(Box::new(cls.to_string()));
            }
            params_vec.push(Box::new(limit as i64));
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params_vec.iter()), |r| {
                    Ok(MemoryEntry {
                        scope: r.get(0)?,
                        trust_class: r.get(1)?,
                        key: r.get(2)?,
                        value_blob: r.get(3)?,
                        ttl_expires: r.get(4)?,
                        updated_at: r.get(5)?,
                        tier: MemoryTier::parse(&r.get::<_, String>(6)?)
                            .unwrap_or(MemoryTier::Warm),
                        hits: r.get::<_, i64>(7)?.max(0) as u64,
                        last_used_at: r.get(8)?,
                        created_at: r.get(9)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Prefix-scan keys visible at one of the supplied trust classes.
    /// Backs `list_memory` (no longer a stub post-migration-0035).
    /// Excludes COLD by default — the agent should never see archived
    /// rows in a list. Use `get` for explicit cold lookups.
    pub fn list(
        &self,
        scope: &str,
        trust_classes: &[&str],
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<MemoryRowSummary>, DbError> {
        if trust_classes.is_empty() {
            return Ok(Vec::new());
        }
        self.db.with_conn(|c| {
            let placeholders = (0..trust_classes.len())
                .map(|i| format!("?{}", i + 3))
                .collect::<Vec<_>>()
                .join(",");
            let prefix_pattern = format!("{}%", prefix.replace('%', "\\%"));
            let sql = format!(
                "SELECT scope, trust_class, key, tier, hits, last_used_at, updated_at \
                 FROM memory_entries \
                 WHERE scope = ?1 AND key LIKE ?2 ESCAPE '\\' AND tier <> 'cold' \
                   AND trust_class IN ({}) \
                 ORDER BY COALESCE(last_used_at, updated_at) DESC \
                 LIMIT ?{}",
                placeholders,
                trust_classes.len() + 3
            );
            let mut stmt = c.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(scope.to_owned()),
                Box::new(prefix_pattern),
            ];
            for cls in trust_classes {
                params_vec.push(Box::new(cls.to_string()));
            }
            params_vec.push(Box::new(limit as i64));
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params_vec.iter()), |r| {
                    Ok(MemoryRowSummary {
                        scope: r.get(0)?,
                        trust_class: r.get(1)?,
                        key: r.get(2)?,
                        tier: MemoryTier::parse(&r.get::<_, String>(3)?)
                            .unwrap_or(MemoryTier::Warm),
                        hits: r.get::<_, i64>(4)?.max(0) as u64,
                        last_used_at: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Find warm rows that meet a frequency-based promotion bar:
    /// at least `min_hits` reads AND at least one read since
    /// `since_unix`. The promotion sweeper feeds these into
    /// [`PromotionStore::propose`].
    pub fn promotion_candidates(
        &self,
        min_hits: u64,
        since_unix: i64,
        limit: u32,
    ) -> Result<Vec<MemoryRowSummary>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT scope, trust_class, key, tier, hits, last_used_at, updated_at \
                 FROM memory_entries \
                 WHERE tier = 'warm' AND hits >= ?1 AND COALESCE(last_used_at, 0) >= ?2 \
                 ORDER BY hits DESC, last_used_at DESC \
                 LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![min_hits as i64, since_unix, limit as i64], |r| {
                    Ok(MemoryRowSummary {
                        scope: r.get(0)?,
                        trust_class: r.get(1)?,
                        key: r.get(2)?,
                        tier: MemoryTier::parse(&r.get::<_, String>(3)?)
                            .unwrap_or(MemoryTier::Warm),
                        hits: r.get::<_, i64>(4)?.max(0) as u64,
                        last_used_at: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Find HOT rows that haven't been read in `idle_seconds`. The
    /// sweeper proposes demoting these back to warm so the HOT slot
    /// stays focused on currently-relevant context.
    pub fn demotion_candidates(
        &self,
        idle_before_unix: i64,
        limit: u32,
    ) -> Result<Vec<MemoryRowSummary>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT scope, trust_class, key, tier, hits, last_used_at, updated_at \
                 FROM memory_entries \
                 WHERE tier = 'hot' AND COALESCE(last_used_at, updated_at) < ?1 \
                 ORDER BY COALESCE(last_used_at, updated_at) ASC \
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![idle_before_unix, limit as i64], |r| {
                    Ok(MemoryRowSummary {
                        scope: r.get(0)?,
                        trust_class: r.get(1)?,
                        key: r.get(2)?,
                        tier: MemoryTier::parse(&r.get::<_, String>(3)?)
                            .unwrap_or(MemoryTier::Warm),
                        hits: r.get::<_, i64>(4)?.max(0) as u64,
                        last_used_at: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Apply an approved tier transition. Called by the promotion
    /// store's `approve` path — never by the agent directly.
    pub fn set_tier(
        &self,
        scope: &str,
        trust_class: &str,
        key: &str,
        tier: MemoryTier,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE memory_entries SET tier = ?4 \
                  WHERE scope = ?1 AND trust_class = ?2 AND key = ?3",
                params![scope, trust_class, key, tier.as_sql()],
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

    fn fresh() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn entry(scope: &str, trust: &str, key: &str, value: &str, now: i64) -> MemoryEntry {
        MemoryEntry {
            scope: scope.into(),
            trust_class: trust.into(),
            key: key.into(),
            value_blob: value.as_bytes().to_vec(),
            ttl_expires: None,
            updated_at: now,
            tier: MemoryTier::Warm,
            hits: 0,
            last_used_at: None,
            created_at: now,
        }
    }

    #[test]
    fn upsert_and_get_roundtrip_preserves_lifecycle_columns() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let mut e = entry("global", "Controller", "favorite_voice", "bf_emma", 1000);
        e.tier = MemoryTier::Hot;
        store.upsert(&e).unwrap();
        let got = store
            .get("global", "Controller", "favorite_voice")
            .unwrap()
            .unwrap();
        assert_eq!(got.value_blob, b"bf_emma");
        assert_eq!(got.tier, MemoryTier::Hot);
        assert_eq!(got.created_at, 1000);
        assert_eq!(got.hits, 0);
        assert!(got.last_used_at.is_none());
    }

    #[test]
    fn trust_class_scoping_is_enforced_by_primary_key() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        store.upsert(&entry("s", "Controller", "k", "c", 1)).unwrap();
        store
            .upsert(&entry("s", "KnownTrusted", "k", "kt", 1))
            .unwrap();
        let c = store.get("s", "Controller", "k").unwrap().unwrap();
        let kt = store.get("s", "KnownTrusted", "k").unwrap().unwrap();
        assert_eq!(c.value_blob, b"c");
        assert_eq!(kt.value_blob, b"kt");
    }

    /// Adversarial: a missing trust class must not see a
    /// higher-trust value with the same scope/key.
    #[test]
    fn get_does_not_spill_across_trust_classes() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        store
            .upsert(&entry("s", "Controller", "secret", "top", 1))
            .unwrap();
        for level in [
            "Delegated",
            "KnownTrusted",
            "KnownLimited",
            "UnknownPending",
            "Blocked",
        ] {
            assert!(store.get("s", level, "secret").unwrap().is_none());
        }
        assert!(store.get("s", "Controller", "secret").unwrap().is_some());
    }

    #[test]
    fn upsert_overwrites_value_but_preserves_tier_and_hits() {
        // Regression: re-writing the value of a HOT row from the
        // agent's `write_memory` tool must NOT silently demote the
        // tier or zero the hit count. The lifecycle is owned by the
        // promotion store, not by value writes.
        let db = fresh();
        let store = MemoryStore::new(&db);
        let mut e = entry("s", "Controller", "k", "v1", 1);
        e.tier = MemoryTier::Hot;
        store.upsert(&e).unwrap();
        for _ in 0..5 {
            store.bump_hit("s", "Controller", "k", 100).unwrap();
        }
        // Same row, new value, default tier on the supplied entry.
        let mut e2 = entry("s", "Controller", "k", "v2", 2);
        e2.tier = MemoryTier::Warm; // would-be downgrade
        store.upsert(&e2).unwrap();

        let got = store.get("s", "Controller", "k").unwrap().unwrap();
        assert_eq!(got.value_blob, b"v2");
        assert_eq!(got.updated_at, 2);
        assert_eq!(got.tier, MemoryTier::Hot, "tier must survive value overwrite");
        assert_eq!(got.hits, 5, "hit count must survive value overwrite");
    }

    #[test]
    fn bump_hit_increments_and_stamps_last_used_at() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        store.upsert(&entry("s", "Controller", "k", "v", 1)).unwrap();
        let n1 = store.bump_hit("s", "Controller", "k", 1000).unwrap();
        let n2 = store.bump_hit("s", "Controller", "k", 1100).unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 2);
        let got = store.get("s", "Controller", "k").unwrap().unwrap();
        assert_eq!(got.hits, 2);
        assert_eq!(got.last_used_at, Some(1100));
    }

    #[test]
    fn bump_hit_on_missing_row_is_noop_returns_zero() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let n = store.bump_hit("s", "Controller", "missing", 100).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn list_hot_excludes_warm_and_cold_orders_by_recency() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let trust = "Controller";
        // 3 HOT, 1 WARM, 1 COLD — only the HOT come back.
        let mut a = entry(scope, trust, "a", "v", 1);
        a.tier = MemoryTier::Hot;
        a.last_used_at = Some(100);
        let mut b = entry(scope, trust, "b", "v", 1);
        b.tier = MemoryTier::Hot;
        b.last_used_at = Some(300);
        let mut c = entry(scope, trust, "c", "v", 1);
        c.tier = MemoryTier::Hot;
        c.last_used_at = Some(200);
        let mut warm = entry(scope, trust, "warm_row", "v", 1);
        warm.tier = MemoryTier::Warm;
        warm.last_used_at = Some(999);
        let mut cold = entry(scope, trust, "cold_row", "v", 1);
        cold.tier = MemoryTier::Cold;
        cold.last_used_at = Some(999);
        for e in [&a, &b, &c, &warm, &cold] {
            store.upsert(e).unwrap();
        }
        let rows = store.list_hot(scope, &["Controller"], 10).unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.as_str()).collect();
        // Recency-ordered: b (300), c (200), a (100).
        assert_eq!(keys, vec!["b", "c", "a"]);
    }

    #[test]
    fn list_hot_respects_trust_class_chain() {
        // A KnownTrusted caller (read-down chain: KnownTrusted,
        // KnownLimited, UnknownPending) must not see Controller HOT
        // rows even when they share scope.
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let mut ctrl = entry(scope, "Controller", "ctrl_secret", "v", 1);
        ctrl.tier = MemoryTier::Hot;
        ctrl.last_used_at = Some(100);
        let mut kt = entry(scope, "KnownTrusted", "shared", "v", 1);
        kt.tier = MemoryTier::Hot;
        kt.last_used_at = Some(50);
        store.upsert(&ctrl).unwrap();
        store.upsert(&kt).unwrap();
        let rows = store
            .list_hot(scope, &["KnownTrusted", "KnownLimited", "UnknownPending"], 10)
            .unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["shared"]);
    }

    #[test]
    fn list_excludes_cold_returns_warm_and_hot_with_prefix() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let trust = "Controller";
        let mut hot = entry(scope, trust, "pref_voice", "v", 1);
        hot.tier = MemoryTier::Hot;
        let warm = entry(scope, trust, "pref_tone", "v", 1);
        let mut cold = entry(scope, trust, "pref_dead", "v", 1);
        cold.tier = MemoryTier::Cold;
        let unrelated = entry(scope, trust, "other_key", "v", 1);
        for e in [&hot, &warm, &cold, &unrelated] {
            store.upsert(e).unwrap();
        }
        let rows = store
            .list(scope, &["Controller"], "pref_", 10)
            .unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.as_str()).collect();
        // Cold excluded, unrelated excluded, prefix matches kept.
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"pref_voice"));
        assert!(keys.contains(&"pref_tone"));
        assert!(!keys.contains(&"pref_dead"));
        assert!(!keys.contains(&"other_key"));
    }

    /// Adversarial: a key starting with `%` must not be interpreted
    /// as a wildcard match. Escape handling in the LIKE clause has to
    /// hold or the agent could probe outside its intended prefix.
    #[test]
    fn list_prefix_escapes_sql_wildcards() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let trust = "Controller";
        store
            .upsert(&entry(scope, trust, "literal_pct_match", "v", 1))
            .unwrap();
        store
            .upsert(&entry(scope, trust, "%_pct_match", "v", 1))
            .unwrap();
        // Caller asks for a literal "%_" prefix. Without escape, the
        // pattern would be `%_%` and match everything. With escape,
        // only the actual `%_pct_match` row matches.
        let rows = store.list(scope, &["Controller"], "%_", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "%_pct_match");
    }

    #[test]
    fn promotion_candidates_filters_by_hits_and_recency() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let trust = "Controller";
        // 4 hits, recent → candidate
        store.upsert(&entry(scope, trust, "good", "v", 1)).unwrap();
        for _ in 0..4 {
            store.bump_hit(scope, trust, "good", 1000).unwrap();
        }
        // 5 hits, ancient → fails recency
        store.upsert(&entry(scope, trust, "stale", "v", 1)).unwrap();
        for _ in 0..5 {
            store.bump_hit(scope, trust, "stale", 1).unwrap();
        }
        // 1 hit, recent → fails frequency
        store.upsert(&entry(scope, trust, "rare", "v", 1)).unwrap();
        store.bump_hit(scope, trust, "rare", 1000).unwrap();
        // Already HOT → never a promotion candidate
        let mut already_hot = entry(scope, trust, "promoted", "v", 1);
        already_hot.tier = MemoryTier::Hot;
        store.upsert(&already_hot).unwrap();
        for _ in 0..10 {
            store.bump_hit(scope, trust, "promoted", 1000).unwrap();
        }

        let cands = store.promotion_candidates(3, 500, 10).unwrap();
        let keys: Vec<_> = cands.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["good"]);
    }

    #[test]
    fn demotion_candidates_returns_idle_hot_rows_oldest_first() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let trust = "Controller";
        let mut fresh_hot = entry(scope, trust, "fresh", "v", 1);
        fresh_hot.tier = MemoryTier::Hot;
        fresh_hot.last_used_at = Some(900);
        let mut idle_hot = entry(scope, trust, "idle", "v", 1);
        idle_hot.tier = MemoryTier::Hot;
        idle_hot.last_used_at = Some(100);
        let warm = entry(scope, trust, "warm_anyway", "v", 1);
        store.upsert(&fresh_hot).unwrap();
        store.upsert(&idle_hot).unwrap();
        store.upsert(&warm).unwrap();
        let cands = store.demotion_candidates(500, 10).unwrap();
        let keys: Vec<_> = cands.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["idle"]);
    }

    #[test]
    fn set_tier_changes_only_the_targeted_row() {
        let db = fresh();
        let store = MemoryStore::new(&db);
        let scope = "global";
        let trust = "Controller";
        store.upsert(&entry(scope, trust, "a", "v", 1)).unwrap();
        store.upsert(&entry(scope, trust, "b", "v", 1)).unwrap();
        store.set_tier(scope, trust, "a", MemoryTier::Hot).unwrap();
        let a = store.get(scope, trust, "a").unwrap().unwrap();
        let b = store.get(scope, trust, "b").unwrap().unwrap();
        assert_eq!(a.tier, MemoryTier::Hot);
        assert_eq!(b.tier, MemoryTier::Warm);
    }
}
