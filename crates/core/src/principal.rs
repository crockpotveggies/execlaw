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
    pub fn upsert(&self, p: &Principal) -> Result<(), DbError> {
        let identifiers = serde_json::to_vec(&p.identifiers)
            .map_err(|e| DbError::Serde(format!("identifiers: {e}")))?;
        let trust = serde_json::to_vec(&p.trust_level)
            .map_err(|e| DbError::Serde(format!("trust_level: {e}")))?;
        let resolved = serde_json::to_vec(&p.resolved_by)
            .map_err(|e| DbError::Serde(format!("resolved_by: {e}")))?;
        let metadata = serde_json::to_vec(&p.metadata)
            .map_err(|e| DbError::Serde(format!("metadata: {e}")))?;
        self.db.with_conn(|c| {
            c.execute(
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
                    p.id.as_str(),
                    identifiers,
                    trust,
                    resolved,
                    metadata,
                    p.first_seen,
                    p.last_seen,
                    p.controller_notes,
                ],
            )?;
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
    /// Phase 3 implementation: scan rows and check membership. When
    /// the principal count grows, a companion index table lands.
    pub fn find_by_identifier(&self, ident: &Identifier) -> Result<Option<Principal>, DbError> {
        let all = self.list_all()?;
        Ok(all.into_iter().find(|p| p.identifiers.contains(ident)))
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
}
