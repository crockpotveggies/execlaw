//! Trust policy configuration (`config_trust_policy`).
//!
//! Operator-editable rules that govern how the cold-contact ladder
//! and group trust composition behave. The schema is a free-form KV
//! table (see migration 0001), but the keys and value shapes are
//! locked by MIGRATION_PLAN §2.6 + the configurable-defaults table
//! at §850. This module is the typed wrapper that:
//!
//!   * Knows every documented key.
//!   * Validates value shapes on write (booleans, enums, durations).
//!   * Returns documented defaults for keys the operator hasn't set.
//!
//! See `crates/server/src/trust_policy.rs` for the HTTP surface and
//! `web/src/settings/TrustPolicyPage.tsx` for the SPA.

use crate::config::{ConfigKv, ConfigTable};
use crate::db::{Database, DbError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Minimum `trust_hint` value that qualifies a plugin-matched
/// contact for auto-promotion to `KnownTrusted`. Mirrors the values
/// the trust-hint enum can produce (§2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MinTrustHint {
    Contact,
    Colleague,
    Organization,
}

impl MinTrustHint {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Contact => "Contact",
            Self::Colleague => "Colleague",
            Self::Organization => "Organization",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Contact" => Some(Self::Contact),
            "Colleague" => Some(Self::Colleague),
            "Organization" => Some(Self::Organization),
            _ => None,
        }
    }
}

/// How effective trust is computed for groups that mix participants
/// of different trust classes (§2.6.4). Today only `MinWins` is
/// implemented; the enum exists so adding `MaxWins` / `MajorityWins`
/// later is a schema-compatible change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MixedTrustPolicy {
    MinWins,
}

impl MixedTrustPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MinWins => "min_wins",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "min_wins" => Some(Self::MinWins),
            _ => None,
        }
    }
}

/// Resolved policy snapshot. Every field is populated; missing keys
/// fall through to the documented defaults so consumers never need
/// to handle a None.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustPolicy {
    pub auto_trust_contacts: bool,
    pub min_trust_hint_for_auto_trust: MinTrustHint,
    pub mixed_trust_policy: MixedTrustPolicy,
    /// Plugin ids in order of priority. First-match wins on tiebreak
    /// when multiple identity plugins claim the same handle.
    pub identity_plugin_order: Vec<String>,
    /// Default expiry for Delegated grants, e.g. "7d", "12h".
    pub delegated_trust_default_ttl: String,
}

impl TrustPolicy {
    /// Documented defaults from MIGRATION_PLAN §850.
    pub fn defaults() -> Self {
        Self {
            auto_trust_contacts: true,
            min_trust_hint_for_auto_trust: MinTrustHint::Contact,
            mixed_trust_policy: MixedTrustPolicy::MinWins,
            identity_plugin_order: Vec::new(),
            delegated_trust_default_ttl: "7d".to_owned(),
        }
    }
}

/// Operator-supplied form payload. Same shape as `TrustPolicy`. The
/// store validates each field on save; reads always return a fully
/// populated `TrustPolicy` even if the operator has never touched it.
pub type TrustPolicyUpdate = TrustPolicy;

#[derive(Debug, Error)]
pub enum TrustPolicyError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("invalid trust policy: {0}")]
    Invalid(String),
}

const KEY_AUTO_TRUST_CONTACTS: &str = "auto_trust_contacts";
const KEY_MIN_TRUST_HINT: &str = "min_trust_hint_for_auto_trust";
const KEY_MIXED_TRUST_POLICY: &str = "mixed_trust_policy";
const KEY_IDENTITY_PLUGIN_ORDER: &str = "identity_plugin_order";
const KEY_DELEGATED_TTL: &str = "delegated_trust_default_ttl";

/// Validates an operator-supplied TTL string. Accepts `\d+(s|m|h|d)`
/// — same surface the rest of the plan uses for retention windows.
fn validate_ttl(s: &str) -> Result<(), TrustPolicyError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(TrustPolicyError::Invalid(
            "delegated_trust_default_ttl must be non-empty (e.g. '7d')".into(),
        ));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    if !matches!(unit, "s" | "m" | "h" | "d") {
        return Err(TrustPolicyError::Invalid(format!(
            "delegated_trust_default_ttl unit must be one of s/m/h/d, got '{s}'"
        )));
    }
    if num.is_empty() || num.parse::<u64>().is_err() {
        return Err(TrustPolicyError::Invalid(format!(
            "delegated_trust_default_ttl numeric prefix must parse, got '{s}'"
        )));
    }
    Ok(())
}

pub struct TrustPolicyStore<'db> {
    kv: ConfigKv<'db>,
}

impl<'db> TrustPolicyStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self {
            kv: ConfigKv::new(db, ConfigTable::TrustPolicy),
        }
    }

    /// Read every key, falling through to documented defaults on
    /// absence or parse failure. Parse failures additionally surface
    /// as `tracing::warn!` calls (not yet — Phase 9.2 keeps it
    /// silent so a corrupt single key doesn't poison the read path).
    pub fn read(&self) -> Result<TrustPolicy, TrustPolicyError> {
        let mut out = TrustPolicy::defaults();

        if let Some(v) = self.kv.get(KEY_AUTO_TRUST_CONTACTS)? {
            out.auto_trust_contacts = v == "true";
        }
        if let Some(v) = self.kv.get(KEY_MIN_TRUST_HINT)? {
            if let Some(parsed) = MinTrustHint::parse(&v) {
                out.min_trust_hint_for_auto_trust = parsed;
            }
        }
        if let Some(v) = self.kv.get(KEY_MIXED_TRUST_POLICY)? {
            if let Some(parsed) = MixedTrustPolicy::parse(&v) {
                out.mixed_trust_policy = parsed;
            }
        }
        if let Some(v) = self.kv.get(KEY_IDENTITY_PLUGIN_ORDER)? {
            out.identity_plugin_order = v
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(v) = self.kv.get(KEY_DELEGATED_TTL)? {
            if !v.trim().is_empty() {
                out.delegated_trust_default_ttl = v;
            }
        }
        Ok(out)
    }

    /// Atomic write of the full policy. Validates every field BEFORE
    /// touching the DB so a partial write can't leave the policy in
    /// a half-applied state.
    pub fn write(&self, p: &TrustPolicy) -> Result<(), TrustPolicyError> {
        validate_ttl(&p.delegated_trust_default_ttl)?;
        for id in &p.identity_plugin_order {
            if id.contains(',') {
                return Err(TrustPolicyError::Invalid(
                    "identity_plugin_order entries must not contain commas".into(),
                ));
            }
        }

        self.kv.set(
            KEY_AUTO_TRUST_CONTACTS,
            if p.auto_trust_contacts {
                "true"
            } else {
                "false"
            },
        )?;
        self.kv
            .set(KEY_MIN_TRUST_HINT, p.min_trust_hint_for_auto_trust.as_str())?;
        self.kv
            .set(KEY_MIXED_TRUST_POLICY, p.mixed_trust_policy.as_str())?;
        self.kv.set(
            KEY_IDENTITY_PLUGIN_ORDER,
            &p.identity_plugin_order.join(","),
        )?;
        self.kv
            .set(KEY_DELEGATED_TTL, &p.delegated_trust_default_ttl)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn read_unset_returns_documented_defaults() {
        let db = fresh_db();
        let store = TrustPolicyStore::new(&db);
        let p = store.read().unwrap();
        assert!(p.auto_trust_contacts);
        assert_eq!(p.min_trust_hint_for_auto_trust, MinTrustHint::Contact);
        assert_eq!(p.mixed_trust_policy, MixedTrustPolicy::MinWins);
        assert!(p.identity_plugin_order.is_empty());
        assert_eq!(p.delegated_trust_default_ttl, "7d");
    }

    #[test]
    fn write_then_read_round_trips() {
        let db = fresh_db();
        let store = TrustPolicyStore::new(&db);
        let p = TrustPolicy {
            auto_trust_contacts: false,
            min_trust_hint_for_auto_trust: MinTrustHint::Colleague,
            mixed_trust_policy: MixedTrustPolicy::MinWins,
            identity_plugin_order: vec!["plugin-a".into(), "plugin-b".into()],
            delegated_trust_default_ttl: "12h".into(),
        };
        store.write(&p).unwrap();
        let got = store.read().unwrap();
        assert_eq!(got, p);
    }

    #[test]
    fn write_rejects_bad_ttl() {
        let db = fresh_db();
        let store = TrustPolicyStore::new(&db);
        let mut p = TrustPolicy::defaults();
        p.delegated_trust_default_ttl = "7minutes".into(); // unit must be s|m|h|d
        let err = store.write(&p).unwrap_err();
        assert!(matches!(err, TrustPolicyError::Invalid(_)));

        p.delegated_trust_default_ttl = "".into();
        assert!(matches!(
            store.write(&p).unwrap_err(),
            TrustPolicyError::Invalid(_)
        ));

        p.delegated_trust_default_ttl = "abcd".into();
        assert!(matches!(
            store.write(&p).unwrap_err(),
            TrustPolicyError::Invalid(_)
        ));
    }

    #[test]
    fn write_rejects_comma_in_plugin_id() {
        let db = fresh_db();
        let store = TrustPolicyStore::new(&db);
        let mut p = TrustPolicy::defaults();
        p.identity_plugin_order = vec!["bad,id".into()];
        let err = store.write(&p).unwrap_err();
        assert!(matches!(err, TrustPolicyError::Invalid(_)));
    }

    #[test]
    fn read_falls_back_to_default_on_unparseable_value() {
        let db = fresh_db();
        // Corrupt a single key directly via the KV.
        crate::config::ConfigKv::new(&db, ConfigTable::TrustPolicy)
            .set("min_trust_hint_for_auto_trust", "Garbage")
            .unwrap();
        let store = TrustPolicyStore::new(&db);
        let p = store.read().unwrap();
        // Default survives; one bad key doesn't poison the read.
        assert_eq!(p.min_trust_hint_for_auto_trust, MinTrustHint::Contact);
    }

    #[test]
    fn write_then_partial_overwrite_preserves_other_keys() {
        let db = fresh_db();
        let store = TrustPolicyStore::new(&db);
        let mut p = TrustPolicy::defaults();
        p.auto_trust_contacts = false;
        p.delegated_trust_default_ttl = "30m".into();
        store.write(&p).unwrap();

        // Second write tweaks one field; the rest survives verbatim.
        let mut p2 = store.read().unwrap();
        p2.min_trust_hint_for_auto_trust = MinTrustHint::Colleague;
        store.write(&p2).unwrap();

        let got = store.read().unwrap();
        assert!(!got.auto_trust_contacts);
        assert_eq!(got.delegated_trust_default_ttl, "30m");
        assert_eq!(got.min_trust_hint_for_auto_trust, MinTrustHint::Colleague);
    }
}
