//! Shared "admit a sender into the trust ladder" helper for every
//! external transport (web chat, Signal, future bridges).
//!
//! The function lives here rather than in core because it composes
//! three layers that core can't depend on:
//!
//!   * [`PrincipalStore`] (core) — the persisted principal table
//!     and `find_by_identifier` index.
//!   * [`PluginHost::resolve_identity`] (plugin-host) — fanout to
//!     installed identity-provider plugins.
//!   * [`TrustPolicy`] (core) — the operator-editable knobs that
//!     control whether plugin matches admit and at what class.
//!
//! Before this helper landed, `chats.rs` did a hardcoded
//! "elevate to KnownTrusted on any plugin match" path inline, and
//! `signal_inbound.rs` did nothing — a sender known via Google
//! Contacts who messaged the agent on Signal was treated as a
//! cold contact even though the same handle on web auto-trusted.
//! Now both routes admit through the same function and the operator's
//! Trust Policy is load-bearing.
//!
//! The flow:
//!
//!   1. Look up the principal by id (`raw`). Hit → return as-is.
//!   2. Look up by identifier (`{transport, handle}`) — catches the
//!      operator's "My identities" mappings, where the controller
//!      has asserted that `signal:+15551234567` is them. Hit →
//!      return that principal (typically the Controller).
//!   3. Read `TrustPolicy`. If `auto_trust_contacts == false`, skip
//!      to step 5.
//!   4. Call every registered identity-provider plugin via
//!      `PluginHost::resolve_identity`. Translate the transport
//!      to the resolver-kind plugins declare in their `[identity_provider]
//!      .resolves` (e.g. Signal's `+`-prefixed handles map to
//!      `phone`). Pick the highest-confidence match whose
//!      `trust_hint` >= `min_trust_hint_for_auto_trust`. Hit → mint
//!      a new principal at `auto_trust_class` (default: `KnownLimited`).
//!   5. Mint as `UnknownPending`. Caller routes to the cold-contact
//!      gate.

use chrono::Utc;
use execlaw_core::db::DbError;
use execlaw_core::ids::{PluginId, PrincipalId};
use execlaw_core::principal::{Identifier, Principal, PrincipalStore, TrustLevel as CoreTrustLevel};
use execlaw_core::trust_policy::{
    AutoTrustClass, MinTrustHint, TrustPolicy, TrustPolicyStore,
};
use execlaw_plugin_host::PluginHost;
use execlaw_policy::trust::TrustLevel;

#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    #[error("db error: {0}")]
    Db(#[from] DbError),
    #[error("trust policy: {0}")]
    Policy(String),
}

/// Resolve or admit a sender. See module docs for the full flow.
///
/// `principal_id_hint` is the caller's preferred id when minting a
/// fresh principal. For web senders, this is the raw user-supplied
/// id (so a returning sender's id stays stable across sessions).
/// For transport-bridged senders (Signal, etc.) callers pass the
/// canonical transport-prefixed form (`pri_signal_+15551234567`).
///
/// Returns `(Principal, flat_trust_level)`. The caller is
/// responsible for any side effects (binding inserts, conversation
/// resolution) — this helper only owns the principal-table
/// lookup/mint decision.
pub async fn admit_external_principal(
    db: &execlaw_core::db::Database,
    plugin_host: &PluginHost,
    transport: &str,
    handle: &str,
    principal_id_hint: &str,
) -> Result<(Principal, TrustLevel), AdmitError> {
    let store = PrincipalStore::new(db);

    // Step 1 — exact-id hit. Common path for returning senders.
    let pid = PrincipalId::from(principal_id_hint);
    if let Some(existing) = store.get(&pid)? {
        let flat = TrustLevel::parse(existing.trust_level.class_tag())
            .unwrap_or(TrustLevel::UnknownPending);
        return Ok((existing, flat));
    }

    // Step 2 — by-identifier hit. Catches "My identities" mappings:
    // the controller has asserted that `signal:+1...` is them, so
    // an inbound on that handle resolves to the controller without
    // going through the cold-contact gate.
    let ident = Identifier {
        transport: transport.to_owned(),
        handle: handle.to_owned(),
    };
    if let Some(existing) = store.find_by_identifier(&ident)? {
        let flat = TrustLevel::parse(existing.trust_level.class_tag())
            .unwrap_or(TrustLevel::UnknownPending);
        return Ok((existing, flat));
    }

    // Step 3 — load policy. A read failure shouldn't block admission;
    // fall through to defaults so a corrupt config_trust_policy row
    // can't lock every transport out.
    let policy = TrustPolicyStore::new(db)
        .read()
        .unwrap_or_else(|_| TrustPolicy::defaults());

    let now = Utc::now().timestamp();

    // Steps 4 + 5 — plugin-vouched auto-admit, falling through to
    // UnknownPending when the policy disables the path or no provider
    // matches the handle.
    let (trust_level, resolved_by, flat_trust) = if policy.auto_trust_contacts {
        let resolver_kind = resolver_kind_for(transport, handle);
        let matches = plugin_host
            .resolve_identity(&resolver_kind, handle)
            .await;
        classify_matches(&matches, &policy, now)
    } else {
        unknown_pending(now)
    };

    let principal = Principal {
        id: pid,
        identifiers: vec![ident],
        trust_level,
        resolved_by,
        metadata: serde_json::json!({}),
        first_seen: now,
        last_seen: Some(now),
        controller_notes: None,
    };
    store.upsert(&principal)?;
    Ok((principal, flat_trust))
}

/// Translate `(transport, handle)` to the resolver-kind that
/// identity-provider plugins declare in their `[identity_provider]
/// .resolves` field.
///
/// The mapping reflects the data type, not the transport name —
/// google-contacts resolves `phone` and `email`, not `signal` /
/// `whatsapp` / `telegram`. A Signal handle that's an E.164 number
/// gets presented as `phone`; a future Signal-username handle would
/// stay as `signal_username`. Web stays as `web` (its own stable
/// identifier kind).
pub fn resolver_kind_for(transport: &str, handle: &str) -> String {
    match transport {
        // E.164-style transport handles → present as `phone` so
        // contact plugins (Google Contacts, local address book)
        // match against their stored phone numbers.
        "signal" | "whatsapp" | "sms" | "telegram" if handle.starts_with('+') => "phone".to_owned(),
        // Email-shaped transport handles → `email`.
        "email" => "email".to_owned(),
        // Everything else: surface the transport name verbatim and
        // let plugins decide whether they handle it.
        other => other.to_owned(),
    }
}

/// Pure: distill plugin matches into a `TrustLevel`, applying the
/// operator's `min_trust_hint_for_auto_trust` and `auto_trust_class`
/// knobs. Public so tests can pin every branch.
pub fn classify_matches(
    matches: &[serde_json::Value],
    policy: &TrustPolicy,
    now: i64,
) -> (CoreTrustLevel, Vec<PluginId>, TrustLevel) {
    let min_rank = trust_hint_rank(policy.min_trust_hint_for_auto_trust);
    let best = matches
        .iter()
        .filter(|m| {
            let hint = m.get("trust_hint").and_then(|v| v.as_str()).unwrap_or("");
            parse_trust_hint(hint)
                .map(trust_hint_rank)
                .map(|r| r >= min_rank)
                .unwrap_or(false)
        })
        .max_by(|a, b| {
            let ac = a.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let bc = b.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
            ac.partial_cmp(&bc).unwrap_or(std::cmp::Ordering::Equal)
        });

    match best {
        Some(m) => {
            let resolvers = m
                .get("resolved_by")
                .and_then(|v| v.as_str())
                .map(|s| vec![PluginId::from(s)])
                .unwrap_or_default();
            let (core_level, flat) = match policy.auto_trust_class {
                AutoTrustClass::KnownTrusted => (
                    CoreTrustLevel::KnownTrusted {
                        resolvers: resolvers.clone(),
                        approved_by: PrincipalId::from("identity_provider_auto_trust"),
                        approved_at: now,
                    },
                    TrustLevel::KnownTrusted,
                ),
                AutoTrustClass::KnownLimited => (
                    CoreTrustLevel::KnownLimited {
                        resolvers: resolvers.clone(),
                        // Empty allowed_topics + None allowed_tools
                        // means "fall through to the policy engine's
                        // KnownLimited capability set" — currently
                        // `messaging.reply_current_transport` only.
                        // Operators who want to broaden this can
                        // promote the principal manually.
                        allowed_topics: Vec::new(),
                        allowed_tools: None,
                    },
                    TrustLevel::KnownLimited,
                ),
            };
            (core_level, resolvers, flat)
        }
        None => unknown_pending(now),
    }
}

fn unknown_pending(now: i64) -> (CoreTrustLevel, Vec<PluginId>, TrustLevel) {
    (
        CoreTrustLevel::UnknownPending {
            first_seen: now,
            notification_event_seq: None,
        },
        Vec::new(),
        TrustLevel::UnknownPending,
    )
}

/// Parse the trust-hint string an identity-provider plugin returns
/// into its enum form. Unknown / missing → `None` so the filter
/// above drops the match (no opinion = doesn't qualify).
fn parse_trust_hint(s: &str) -> Option<MinTrustHint> {
    // The full ladder includes "Family"/"Friend" but auto-trust
    // gating only ranks Contact/Colleague/Organization (per the
    // policy schema). We collapse Family/Friend to Contact's rank
    // so a plugin that tags more specifically still admits, while
    // Unknown stays out.
    match s {
        "Contact" | "Family" | "Friend" => Some(MinTrustHint::Contact),
        "Colleague" => Some(MinTrustHint::Colleague),
        "Organization" => Some(MinTrustHint::Organization),
        _ => None,
    }
}

/// Numeric rank so `>=` makes sense for the gate. Higher = more
/// trusted.
fn trust_hint_rank(h: MinTrustHint) -> u32 {
    match h {
        MinTrustHint::Contact => 1,
        MinTrustHint::Colleague => 2,
        MinTrustHint::Organization => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::principal::PrincipalStore;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn fresh_host(db: &Database) -> PluginHost {
        let registry = execlaw_plugin_host::hook_registry::HookRegistry::default();
        let stage = tempfile::tempdir().unwrap().keep();
        PluginHost::new(db.clone(), registry, stage)
    }

    fn match_json(trust_hint: &str, confidence: f64, plugin: &str) -> serde_json::Value {
        serde_json::json!({
            "trust_hint": trust_hint,
            "confidence": confidence,
            "resolved_by": plugin,
        })
    }

    #[test]
    fn classify_no_matches_returns_unknown_pending() {
        let p = TrustPolicy::defaults();
        let (core, by, flat) = classify_matches(&[], &p, 100);
        assert!(matches!(core, CoreTrustLevel::UnknownPending { .. }));
        assert!(by.is_empty());
        assert_eq!(flat, TrustLevel::UnknownPending);
    }

    #[test]
    fn classify_picks_highest_confidence_above_min_hint() {
        let p = TrustPolicy::defaults(); // min=Contact, class=KnownLimited
        let matches = vec![
            match_json("Contact", 0.7, "addrbook"),
            match_json("Colleague", 0.9, "google-contacts"),
        ];
        let (core, by, flat) = classify_matches(&matches, &p, 100);
        assert!(matches!(core, CoreTrustLevel::KnownLimited { .. }));
        assert_eq!(by, vec![PluginId::from("google-contacts")]);
        assert_eq!(flat, TrustLevel::KnownLimited);
    }

    #[test]
    fn classify_drops_matches_below_min_hint() {
        let mut p = TrustPolicy::defaults();
        p.min_trust_hint_for_auto_trust = MinTrustHint::Organization;
        // Only Contact-class match available — should not admit.
        let matches = vec![match_json("Contact", 0.99, "addrbook")];
        let (core, _by, flat) = classify_matches(&matches, &p, 100);
        assert!(matches!(core, CoreTrustLevel::UnknownPending { .. }));
        assert_eq!(flat, TrustLevel::UnknownPending);
    }

    #[test]
    fn classify_drops_unknown_trust_hint() {
        let p = TrustPolicy::defaults();
        let matches = vec![match_json("Unknown", 0.99, "noisy-plugin")];
        let (core, _by, flat) = classify_matches(&matches, &p, 100);
        assert!(matches!(core, CoreTrustLevel::UnknownPending { .. }));
        assert_eq!(flat, TrustLevel::UnknownPending);
    }

    #[test]
    fn classify_respects_auto_trust_class_knowntrusted() {
        let mut p = TrustPolicy::defaults();
        p.auto_trust_class = AutoTrustClass::KnownTrusted;
        let matches = vec![match_json("Contact", 0.9, "addrbook")];
        let (core, _by, flat) = classify_matches(&matches, &p, 100);
        assert!(matches!(core, CoreTrustLevel::KnownTrusted { .. }));
        assert_eq!(flat, TrustLevel::KnownTrusted);
    }

    #[test]
    fn resolver_kind_maps_signal_e164_to_phone() {
        assert_eq!(resolver_kind_for("signal", "+15551234567"), "phone");
        assert_eq!(resolver_kind_for("whatsapp", "+15551234567"), "phone");
        assert_eq!(resolver_kind_for("sms", "+15551234567"), "phone");
        // Non-E.164 Signal handle (future username) keeps transport
        // name; plugins that opt in handle it.
        assert_eq!(resolver_kind_for("signal", "alice"), "signal");
        assert_eq!(resolver_kind_for("email", "a@b.c"), "email");
        assert_eq!(resolver_kind_for("web", "user-1"), "web");
    }

    #[tokio::test]
    async fn admit_returns_existing_principal_by_id() {
        let db = fresh_db();
        let host = fresh_host(&db);
        let store = PrincipalStore::new(&db);
        let p = Principal {
            id: PrincipalId::from("user-42"),
            identifiers: Vec::new(),
            trust_level: CoreTrustLevel::KnownTrusted {
                resolvers: Vec::new(),
                approved_by: PrincipalId::from("controller"),
                approved_at: 0,
            },
            resolved_by: Vec::new(),
            metadata: serde_json::json!({}),
            first_seen: 0,
            last_seen: None,
            controller_notes: None,
        };
        store.upsert(&p).unwrap();

        let (got, flat) = admit_external_principal(&db, &host, "web", "user-42", "user-42")
            .await
            .unwrap();
        assert_eq!(got.id.as_str(), "user-42");
        assert_eq!(flat, TrustLevel::KnownTrusted);
    }

    #[tokio::test]
    async fn admit_resolves_via_my_identities_mapping() {
        // The controller registered `signal:+15551234567` as one of
        // their identifiers. When an inbound Signal message arrives,
        // the helper must resolve to the controller — not mint a
        // new UnknownPending principal.
        let db = fresh_db();
        let host = fresh_host(&db);
        let store = PrincipalStore::new(&db);
        let controller = Principal {
            id: PrincipalId::from("controller-x"),
            identifiers: vec![Identifier {
                transport: "signal".into(),
                handle: "+15551234567".into(),
            }],
            trust_level: CoreTrustLevel::Controller,
            resolved_by: Vec::new(),
            metadata: serde_json::json!({}),
            first_seen: 0,
            last_seen: None,
            controller_notes: None,
        };
        store.upsert(&controller).unwrap();

        let (got, flat) = admit_external_principal(
            &db,
            &host,
            "signal",
            "+15551234567",
            // Hint id is the canonical transport-prefixed form;
            // the `find_by_identifier` step short-circuits before it
            // matters.
            "pri_signal_+15551234567",
        )
        .await
        .unwrap();
        assert_eq!(got.id.as_str(), "controller-x");
        assert_eq!(flat, TrustLevel::Controller);
    }

    #[tokio::test]
    async fn admit_mints_unknown_pending_when_no_match() {
        let db = fresh_db();
        let host = fresh_host(&db);
        let (got, flat) = admit_external_principal(
            &db,
            &host,
            "signal",
            "+19998887777",
            "pri_signal_+19998887777",
        )
        .await
        .unwrap();
        assert_eq!(got.id.as_str(), "pri_signal_+19998887777");
        assert!(matches!(got.trust_level, CoreTrustLevel::UnknownPending { .. }));
        assert_eq!(flat, TrustLevel::UnknownPending);
        // Identifier was written so the next inbound finds it via
        // exact-id hit (step 1).
        assert_eq!(got.identifiers.len(), 1);
        assert_eq!(got.identifiers[0].transport, "signal");
        assert_eq!(got.identifiers[0].handle, "+19998887777");
    }
}
