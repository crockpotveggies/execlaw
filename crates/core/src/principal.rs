//! Principal + trust ladder (§2.6, §2.14).
//!
//! The `principals` table persists everything the control plane knows
//! about a participant: their identifiers (transport-scoped handles),
//! their current `TrustLevel` (with full variant data — not just the
//! class tag), which identity-provider plugins resolved them, free-
//! form metadata, and a first/last-seen timestamp pair.
//!
//! Load/save round-trips the rich `TrustLevel` via JSON so a later
//! Phase-3 schema edit doesn't force a migration — we just serialize
//! the enum whole.

use crate::db::{Database, DbError};
use crate::ids::{PluginId, PrincipalId};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Transport-specific identifier, e.g. `signal:+15551234567`,
/// `email:a@b.com`, `web:sess-xyz`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identifier {
    pub transport: String,
    pub handle: String,
}

/// Trust hint published by identity-provider plugins (§2.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustHint {
    Contact,
    Colleague,
    Family,
    Organization,
    Unknown,
}

/// Capability scope for a `Delegated` trust grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityScope {
    pub capabilities: Vec<String>,
}

/// Trust ladder (§2.6).
///
/// `Blocked` is a **universal** state — it applies to previously-unknown
/// contacts AND to previously-trusted principals the controller later
/// decides to block (§0 memory: `project_locked_decisions_2026_04_23.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustLevel {
    Controller,
    Delegated {
        by: PrincipalId,
        scope: CapabilityScope,
        expires_at: Option<i64>,
    },
    KnownTrusted {
        resolvers: Vec<PluginId>,
        approved_by: PrincipalId,
        approved_at: i64,
    },
    KnownLimited {
        resolvers: Vec<PluginId>,
        allowed_topics: Vec<String>,
        allowed_tools: Option<Vec<String>>,
    },
    UnknownPending {
        first_seen: i64,
        notification_event_seq: Option<i64>,
    },
    Blocked {
        blocked_by: PrincipalId,
        blocked_at: i64,
        reason: Option<String>,
    },
}

impl TrustLevel {
    /// Short machine-readable tag for the `principals.trust_class` column
    /// and for policy-engine matching.
    pub fn class_tag(&self) -> &'static str {
        match self {
            TrustLevel::Controller => "Controller",
            TrustLevel::Delegated { .. } => "Delegated",
            TrustLevel::KnownTrusted { .. } => "KnownTrusted",
            TrustLevel::KnownLimited { .. } => "KnownLimited",
            TrustLevel::UnknownPending { .. } => "UnknownPending",
            TrustLevel::Blocked { .. } => "Blocked",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub id: PrincipalId,
    pub identifiers: Vec<Identifier>,
    pub trust_level: TrustLevel,
    pub resolved_by: Vec<PluginId>,
    pub metadata: serde_json::Value,
    pub first_seen: i64,
    pub last_seen: Option<i64>,
    pub controller_notes: Option<String>,
}

// ---------------------------------------------------------------------------
// PrincipalStore — persistence layer for the `principals` table (§2.6).
// ---------------------------------------------------------------------------

/// SQLite-backed store for [`Principal`] rows.
///
/// Load/save serializes the rich `TrustLevel` variant (including
/// `Delegated { by, scope, expires_at }` data etc.) into
/// `trust_level_json` as JSON. A lightweight `class_tag()` column
/// isn't stored separately — every downstream consumer already calls
/// `principal.trust_level.class_tag()` to get the flat string form.
pub struct PrincipalStore<'db> {
    db: &'db Database,
}

impl<'db> PrincipalStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Upsert a principal. Used both for first-observation (new
    /// UnknownPending from an unknown sender) and for trust changes
    /// (controller admits via approval flow).
    ///
    /// Migration 0004 added the `principal_identifiers` index table so
    /// `find_by_identifier` is O(1). We keep it in sync here by
    /// (re-)materialising every identifier as a row inside a single
    /// transaction. Delete-then-insert is simpler than a diff-aware
    /// merge and the identifier set is tiny (typically 1-4 per
    /// principal). The CASCADE on `principal_identifiers.principal_id`
    /// means a `PrincipalStore::delete` later removes the index rows
    /// too without an explicit cleanup.
    pub fn upsert(&self, p: &Principal) -> Result<(), DbError> {
        let identifiers = serde_json::to_vec(&p.identifiers)
            .map_err(|e| DbError::Serde(format!("identifiers: {e}")))?;
        let trust = serde_json::to_vec(&p.trust_level)
            .map_err(|e| DbError::Serde(format!("trust_level: {e}")))?;
        let resolved = serde_json::to_vec(&p.resolved_by)
            .map_err(|e| DbError::Serde(format!("resolved_by: {e}")))?;
        let metadata = serde_json::to_vec(&p.metadata)
            .map_err(|e| DbError::Serde(format!("metadata: {e}")))?;
        let pid = p.id.as_str().to_owned();
        let idents_for_index: Vec<(String, String)> = p
            .identifiers
            .iter()
            .map(|i| (i.transport.clone(), i.handle.clone()))
            .collect();
        let last_seen = p.last_seen;
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO principals \
                 (id, identifiers_json, trust_level_json, resolved_by_json, metadata_json, \
                  first_seen, last_seen, controller_notes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET \
                     identifiers_json = excluded.identifiers_json, \
                     trust_level_json = excluded.trust_level_json, \
                     resolved_by_json = excluded.resolved_by_json, \
                     metadata_json = excluded.metadata_json, \
                     last_seen = excluded.last_seen, \
                     controller_notes = excluded.controller_notes",
                params![
                    pid,
                    identifiers,
                    trust,
                    resolved,
                    metadata,
                    p.first_seen,
                    p.last_seen,
                    p.controller_notes,
                ],
            )?;
            // Resync the identifier index. Cheap because the table
            // is typically empty or contains 1-2 stale rows for this
            // principal; a 500-contact install does ~500 of these
            // total, not per-call.
            tx.execute(
                "DELETE FROM principal_identifiers WHERE principal_id = ?1",
                params![pid],
            )?;
            if !idents_for_index.is_empty() {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO principal_identifiers \
                     (transport, handle, principal_id, last_seen) \
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (transport, handle) in &idents_for_index {
                    stmt.execute(params![transport, handle, pid, last_seen])?;
                }
            }
            Ok(())
        })
    }

    /// Load a principal by id. Returns `None` if no row exists.
    #[allow(clippy::type_complexity)]
    pub fn get(&self, id: &PrincipalId) -> Result<Option<Principal>, DbError> {
        self.db.with_conn(|c| {
            let got: Option<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                i64,
                Option<i64>,
                Option<String>,
            )> = c
                .query_row(
                    "SELECT identifiers_json, trust_level_json, resolved_by_json, metadata_json, \
                            first_seen, last_seen, controller_notes \
                     FROM principals WHERE id = ?1",
                    params![id.as_str()],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .ok();
            let Some((idents, trust, resolved, meta, first, last, notes)) = got else {
                return Ok(None);
            };
            Ok(Some(Principal {
                id: id.clone(),
                identifiers: serde_json::from_slice(&idents)
                    .map_err(|e| DbError::Serde(format!("identifiers: {e}")))?,
                trust_level: serde_json::from_slice(&trust)
                    .map_err(|e| DbError::Serde(format!("trust_level: {e}")))?,
                resolved_by: serde_json::from_slice(&resolved)
                    .map_err(|e| DbError::Serde(format!("resolved_by: {e}")))?,
                metadata: serde_json::from_slice(&meta)
                    .map_err(|e| DbError::Serde(format!("metadata: {e}")))?,
                first_seen: first,
                last_seen: last,
                controller_notes: notes,
            }))
        })
    }

    /// Find a principal by one of its transport-scoped identifiers.
    /// Used on inbound transport events to resolve `(signal, +1555...)`
    /// → an existing Principal, if we've seen them before.
    ///
    /// 2026-05-14 — migrated from the O(N) `list_all` scan to an
    /// O(1) PK lookup against `principal_identifiers` (migration
    /// 0004). The scan was the secondary cost in the Signal-latency
    /// investigation: every external inbound calls this through
    /// `admit_external_principal`, and a 500-contact install was
    /// paying a full principal-table read where the answer was a
    /// single PK probe. When multiple principals share the same
    /// identifier (a stale UnknownPending shadowing a Controller —
    /// the reconcile case), this returns ONE of them deterministically
    /// (the index has no row-order guarantee but a single match is
    /// enough for the callers of this method — they all just want
    /// "is this handle known? if so, take whichever principal claims
    /// it"). Use `find_all_by_identifier` when you need every claimant.
    pub fn find_by_identifier(&self, ident: &Identifier) -> Result<Option<Principal>, DbError> {
        let transport = ident.transport.clone();
        let handle = ident.handle.clone();
        let pid: Option<String> = self.db.with_conn(|c| {
            let got: Option<String> = c
                .query_row(
                    "SELECT principal_id FROM principal_identifiers \
                     WHERE transport = ?1 AND handle = ?2 LIMIT 1",
                    params![transport, handle],
                    |r| r.get::<_, String>(0),
                )
                .ok();
            Ok(got)
        })?;
        match pid {
            Some(id) => self.get(&PrincipalId::from(id)),
            None => Ok(None),
        }
    }

    /// Find every principal that carries the given identifier. Used
    /// by reconcile to detect stale UnknownPending rows shadowing a
    /// controller-asserted "My identities" mapping (or any other
    /// higher-trust principal that owns the same handle).
    ///
    /// 2026-05-14 — now uses the `principal_identifiers` index for
    /// the (transport, handle) → principal_id lookup, then fetches
    /// each principal individually. The expected fan-out is 1-2 rows
    /// per identifier (UnknownPending + canonical winner, briefly,
    /// inside the reconcile window) so the round-trip cost is
    /// negligible compared to the scan it replaces. Results are
    /// returned in id order for deterministic dedupe in callers.
    pub fn find_all_by_identifier(&self, ident: &Identifier) -> Result<Vec<Principal>, DbError> {
        let transport = ident.transport.clone();
        let handle = ident.handle.clone();
        let ids: Vec<String> = self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT principal_id FROM principal_identifiers \
                 WHERE transport = ?1 AND handle = ?2 \
                 ORDER BY principal_id ASC",
            )?;
            let rows = stmt.query_map(params![transport, handle], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?;
        let mut principals = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(p) = self.get(&PrincipalId::from(id))? {
                principals.push(p);
            }
        }
        Ok(principals)
    }

    /// Delete a principal by id. Returns `true` when a row was
    /// removed. Use only after the caller has rebound any
    /// `state_transport_bindings` and `state_conversations` that
    /// referenced this principal's group — `state_principal_group_members`
    /// rows are NOT touched here (the caller drops the group via
    /// [`PrincipalGroupStore::delete`] which cascades members).
    pub fn delete(&self, id: &PrincipalId) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute("DELETE FROM principals WHERE id = ?1", params![id.as_str()])?;
            Ok(n > 0)
        })
    }

    /// Enumerate every principal. Small-table operation — the
    /// principals table grows with contacts, not messages.
    pub fn list_all(&self) -> Result<Vec<Principal>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, identifiers_json, trust_level_json, resolved_by_json, metadata_json, \
                        first_seen, last_seen, controller_notes \
                 FROM principals ORDER BY first_seen ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, idents, trust, resolved, meta, first, last, notes) = row?;
                out.push(Principal {
                    id: PrincipalId::from(id),
                    identifiers: serde_json::from_slice(&idents)
                        .map_err(|e| DbError::Serde(format!("identifiers: {e}")))?,
                    trust_level: serde_json::from_slice(&trust)
                        .map_err(|e| DbError::Serde(format!("trust_level: {e}")))?,
                    resolved_by: serde_json::from_slice(&resolved)
                        .map_err(|e| DbError::Serde(format!("resolved_by: {e}")))?,
                    metadata: serde_json::from_slice(&meta)
                        .map_err(|e| DbError::Serde(format!("metadata: {e}")))?,
                    first_seen: first,
                    last_seen: last,
                    controller_notes: notes,
                });
            }
            Ok(out)
        })
    }

    /// Transition a principal's trust level. Writes the new
    /// `TrustLevel` and bumps `last_seen`. The caller is responsible
    /// for committing a `TrustChanged` event separately so the audit
    /// trail captures who / when / why.
    pub fn set_trust(&self, id: &PrincipalId, new_level: TrustLevel) -> Result<(), DbError> {
        let Some(mut principal) = self.get(id)? else {
            return Err(DbError::Invariant(format!(
                "cannot set_trust: principal '{}' not found",
                id.as_str()
            )));
        };
        principal.trust_level = new_level;
        principal.last_seen = Some(chrono::Utc::now().timestamp());
        self.upsert(&principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_tag_covers_all_variants() {
        // Compile-time list so we don't forget to update the tag on new
        // variants.
        assert_eq!(TrustLevel::Controller.class_tag(), "Controller");
        assert_eq!(
            TrustLevel::Blocked {
                blocked_by: PrincipalId::from("c"),
                blocked_at: 0,
                reason: None,
            }
            .class_tag(),
            "Blocked"
        );
        assert_eq!(
            TrustLevel::UnknownPending {
                first_seen: 0,
                notification_event_seq: None,
            }
            .class_tag(),
            "UnknownPending"
        );
    }

    #[test]
    fn trust_level_json_roundtrips() {
        let t = TrustLevel::KnownTrusted {
            resolvers: vec![PluginId::from("google-contacts")],
            approved_by: PrincipalId::from("controller"),
            approved_at: 12345,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TrustLevel = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
    }

    /// Every TrustLevel variant must produce a distinct class tag. If
    /// a future variant shadows an existing tag, the DB's trust_class
    /// column becomes ambiguous.
    #[test]
    fn every_variant_has_a_unique_class_tag() {
        let variants = [
            TrustLevel::Controller,
            TrustLevel::Delegated {
                by: PrincipalId::from("c"),
                scope: CapabilityScope {
                    capabilities: vec![],
                },
                expires_at: None,
            },
            TrustLevel::KnownTrusted {
                resolvers: vec![],
                approved_by: PrincipalId::from("c"),
                approved_at: 0,
            },
            TrustLevel::KnownLimited {
                resolvers: vec![],
                allowed_topics: vec![],
                allowed_tools: None,
            },
            TrustLevel::UnknownPending {
                first_seen: 0,
                notification_event_seq: None,
            },
            TrustLevel::Blocked {
                blocked_by: PrincipalId::from("c"),
                blocked_at: 0,
                reason: None,
            },
        ];
        let tags: Vec<&'static str> = variants.iter().map(|v| v.class_tag()).collect();
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), variants.len(), "class tags collide: {tags:?}");
        // Exact expected tags.
        assert_eq!(
            tags,
            vec![
                "Controller",
                "Delegated",
                "KnownTrusted",
                "KnownLimited",
                "UnknownPending",
                "Blocked",
            ]
        );
    }

    /// TrustHint enum round-trips through JSON — important because it's
    /// published by identity-provider plugins as untrusted input.
    #[test]
    fn trust_hint_roundtrips_all_variants() {
        for h in [
            TrustHint::Contact,
            TrustHint::Colleague,
            TrustHint::Family,
            TrustHint::Organization,
            TrustHint::Unknown,
        ] {
            let s = serde_json::to_string(&h).unwrap();
            let back: TrustHint = serde_json::from_str(&s).unwrap();
            assert_eq!(back, h);
        }
    }

    /// Identifier hash-equality — two Identifiers with the same
    /// transport+handle must match; differing transport must not.
    #[test]
    fn identifier_equality_and_hash_are_value_based() {
        use std::collections::HashSet;
        let a = Identifier {
            transport: "signal".into(),
            handle: "+15551234567".into(),
        };
        let b = Identifier {
            transport: "signal".into(),
            handle: "+15551234567".into(),
        };
        let c = Identifier {
            transport: "email".into(),
            handle: "+15551234567".into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    // ---- PrincipalStore round-trip tests -----------------------------------

    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn mk_principal(id: &str, trust: TrustLevel) -> Principal {
        Principal {
            id: PrincipalId::from(id),
            identifiers: vec![Identifier {
                transport: "web".into(),
                handle: format!("web:{id}"),
            }],
            trust_level: trust,
            resolved_by: vec![],
            metadata: serde_json::json!({}),
            first_seen: 1,
            last_seen: None,
            controller_notes: None,
        }
    }

    #[test]
    fn principal_upsert_and_get_roundtrips_rich_trust_level() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        // Delegated trust carries nested data — round-trip it through JSON.
        let p = mk_principal(
            "p1",
            TrustLevel::Delegated {
                by: PrincipalId::from("controller"),
                scope: CapabilityScope {
                    capabilities: vec!["memory.read".into(), "tools.safe".into()],
                },
                expires_at: Some(2_000_000),
            },
        );
        store.upsert(&p).unwrap();
        let got = store.get(&p.id).unwrap().unwrap();
        match got.trust_level {
            TrustLevel::Delegated {
                by,
                scope,
                expires_at,
            } => {
                assert_eq!(by.as_str(), "controller");
                assert_eq!(scope.capabilities.len(), 2);
                assert_eq!(expires_at, Some(2_000_000));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn find_by_identifier_returns_match() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        store
            .upsert(&mk_principal("p1", TrustLevel::Controller))
            .unwrap();
        store
            .upsert(&mk_principal(
                "p2",
                TrustLevel::UnknownPending {
                    first_seen: 0,
                    notification_event_seq: None,
                },
            ))
            .unwrap();

        let hit = store
            .find_by_identifier(&Identifier {
                transport: "web".into(),
                handle: "web:p2".into(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(hit.id.as_str(), "p2");

        let miss = store
            .find_by_identifier(&Identifier {
                transport: "signal".into(),
                handle: "+15550000000".into(),
            })
            .unwrap();
        assert!(miss.is_none());
    }

    #[test]
    fn list_all_orders_by_first_seen() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let mut early = mk_principal("early", TrustLevel::Controller);
        early.first_seen = 100;
        let mut late = mk_principal("late", TrustLevel::Controller);
        late.first_seen = 200;
        // Insert out of order.
        store.upsert(&late).unwrap();
        store.upsert(&early).unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id.as_str(), "early");
        assert_eq!(all[1].id.as_str(), "late");
    }

    /// `set_trust` transitions the variant AND bumps `last_seen`.
    #[test]
    fn set_trust_updates_level_and_last_seen() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        store
            .upsert(&mk_principal(
                "p1",
                TrustLevel::UnknownPending {
                    first_seen: 0,
                    notification_event_seq: None,
                },
            ))
            .unwrap();
        store
            .set_trust(
                &PrincipalId::from("p1"),
                TrustLevel::KnownTrusted {
                    resolvers: vec![PluginId::from("identity-local")],
                    approved_by: PrincipalId::from("controller"),
                    approved_at: 1_000,
                },
            )
            .unwrap();
        let got = store.get(&PrincipalId::from("p1")).unwrap().unwrap();
        assert!(matches!(got.trust_level, TrustLevel::KnownTrusted { .. }));
        assert!(got.last_seen.is_some());
    }

    /// Adversarial: `set_trust` on a missing principal must return
    /// an Invariant error (not silently upsert a half-constructed row).
    #[test]
    fn set_trust_on_missing_principal_is_error() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let err = store
            .set_trust(&PrincipalId::from("nonexistent"), TrustLevel::Controller)
            .unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    /// Upsert is idempotent — calling it twice with the same data
    /// produces exactly one row.
    #[test]
    fn upsert_is_idempotent() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let p = mk_principal("p1", TrustLevel::Controller);
        store.upsert(&p).unwrap();
        store.upsert(&p).unwrap();
        assert_eq!(store.list_all().unwrap().len(), 1);
    }

    // ---------------------------------------------------------------------
    // principal_identifiers index (migration 0004) — load-bearing for the
    // local-principal-cache policy that makes `admit_external_principal`
    // O(1) for previously-seen senders.
    // ---------------------------------------------------------------------

    /// Helper: count the index rows for one (transport, handle).
    fn index_row_count(db: &Database, transport: &str, handle: &str) -> i64 {
        db.with_conn(|c| {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM principal_identifiers \
                     WHERE transport = ?1 AND handle = ?2",
                    params![transport, handle],
                    |r| r.get(0),
                )
                .unwrap();
            Ok(n)
        })
        .unwrap()
    }

    #[test]
    fn upsert_populates_principal_identifiers_index() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let mut p = mk_principal("p-multi", TrustLevel::Controller);
        // Two identifiers — Signal + email — to verify both land in
        // the index.
        p.identifiers = vec![
            Identifier {
                transport: "signal".into(),
                handle: "+15551112222".into(),
            },
            Identifier {
                transport: "email".into(),
                handle: "alice@example.com".into(),
            },
        ];
        store.upsert(&p).unwrap();
        assert_eq!(index_row_count(&db, "signal", "+15551112222"), 1);
        assert_eq!(index_row_count(&db, "email", "alice@example.com"), 1);
    }

    #[test]
    fn upsert_resyncs_index_when_identifiers_change() {
        // The cache must follow the principal's current identifier set.
        // Operator removes an old phone number from "My identities" →
        // the next upsert must drop the stale index row.
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let mut p = mk_principal("p-resync", TrustLevel::Controller);
        p.identifiers = vec![Identifier {
            transport: "signal".into(),
            handle: "+15551110000".into(),
        }];
        store.upsert(&p).unwrap();
        assert_eq!(index_row_count(&db, "signal", "+15551110000"), 1);

        // Swap the identifier.
        p.identifiers = vec![Identifier {
            transport: "signal".into(),
            handle: "+15552220000".into(),
        }];
        store.upsert(&p).unwrap();
        assert_eq!(
            index_row_count(&db, "signal", "+15551110000"),
            0,
            "old identifier must be removed from the index after upsert with new set",
        );
        assert_eq!(index_row_count(&db, "signal", "+15552220000"), 1);
    }

    #[test]
    fn delete_principal_cascades_to_identifier_index() {
        // FK CASCADE on principal_identifiers.principal_id means a
        // principal delete also removes its index rows. Pin the
        // contract so a future schema edit can't regress it.
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let mut p = mk_principal("p-cascade", TrustLevel::Controller);
        p.identifiers = vec![Identifier {
            transport: "signal".into(),
            handle: "+19990001111".into(),
        }];
        store.upsert(&p).unwrap();
        assert_eq!(index_row_count(&db, "signal", "+19990001111"), 1);

        let removed = store.delete(&PrincipalId::from("p-cascade")).unwrap();
        assert!(removed);
        assert_eq!(
            index_row_count(&db, "signal", "+19990001111"),
            0,
            "delete must cascade through the FK to the identifier index",
        );
    }

    #[test]
    fn find_by_identifier_returns_known_principal_via_index() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let mut p = mk_principal(
            "p-lookup",
            TrustLevel::KnownLimited {
                resolvers: vec![],
                allowed_topics: vec![],
                allowed_tools: None,
            },
        );
        p.identifiers = vec![Identifier {
            transport: "signal".into(),
            handle: "+13334445555".into(),
        }];
        store.upsert(&p).unwrap();

        let got = store
            .find_by_identifier(&Identifier {
                transport: "signal".into(),
                handle: "+13334445555".into(),
            })
            .unwrap()
            .expect("known identifier must resolve to its principal");
        assert_eq!(got.id.as_str(), "p-lookup");
    }

    #[test]
    fn find_by_identifier_miss_for_unseen_handle() {
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let got = store
            .find_by_identifier(&Identifier {
                transport: "signal".into(),
                handle: "+10000000000".into(),
            })
            .unwrap();
        assert!(
            got.is_none(),
            "unseen handle must return None so admit_external_principal falls through to plugin fanout",
        );
    }

    #[test]
    fn find_all_by_identifier_returns_every_claimant_for_reconcile_case() {
        // Reconcile fixture: a stale UnknownPending principal AND a
        // canonical Controller principal both claim the same handle.
        // `find_all_by_identifier` must surface BOTH so the merge
        // logic can pick the winner and clean up the loser.
        let db = fresh_db();
        let store = PrincipalStore::new(&db);
        let mut stale = mk_principal(
            "stale",
            TrustLevel::UnknownPending {
                first_seen: 100,
                notification_event_seq: None,
            },
        );
        stale.identifiers = vec![Identifier {
            transport: "signal".into(),
            handle: "+14155551212".into(),
        }];
        let mut canonical = mk_principal("canonical", TrustLevel::Controller);
        canonical.identifiers = vec![Identifier {
            transport: "signal".into(),
            handle: "+14155551212".into(),
        }];
        store.upsert(&stale).unwrap();
        store.upsert(&canonical).unwrap();

        let claimants = store
            .find_all_by_identifier(&Identifier {
                transport: "signal".into(),
                handle: "+14155551212".into(),
            })
            .unwrap();
        assert_eq!(claimants.len(), 2);
        let ids: std::collections::HashSet<String> =
            claimants.iter().map(|p| p.id.as_str().to_owned()).collect();
        assert!(ids.contains("stale"));
        assert!(ids.contains("canonical"));
    }

    // (The migration-0004 backfill test was retired 2026-05-14 when
    // migrations 2-4 were folded into the baseline. The backfill SQL
    // it pinned no longer ships separately; the baseline now creates
    // `principal_identifiers` directly and `upsert` keeps it in sync.
    // The remaining tests in this module cover the runtime behaviour
    // that matters: upsert-populates-index, resync-on-change,
    // cascade-on-delete, single + multi claimant lookup.)
}
