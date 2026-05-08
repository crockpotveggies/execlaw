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
use execlaw_core::principal::{
    Identifier, Principal, PrincipalStore, TrustLevel as CoreTrustLevel,
};
use execlaw_core::principal_groups::PrincipalGroupStore;
use execlaw_core::transport_bindings::TransportBindingStore;
use execlaw_core::trust_policy::{AutoTrustClass, MinTrustHint, TrustPolicy, TrustPolicyStore};
use execlaw_plugin_host::PluginHost;
use execlaw_policy::trust::TrustLevel;
use rusqlite::params;

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
        let matches = plugin_host.resolve_identity(&resolver_kind, handle).await;
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
/// The mapping is driven by the **handle's shape**, not the
/// transport name — google-contacts resolves `phone` and `email`,
/// not `signal` / `whatsapp` / `telegram` / `imessage` / etc.
/// Looking at the handle keeps this open to any future
/// E.164-shaped transport without code changes.
///
///   * Handle starts with `+` and is otherwise digits/spaces/dashes
///     → `phone` (E.164 by convention).
///   * Handle contains an `@` and has a `.`-separated TLD on the
///     right-hand side → `email`.
///   * Otherwise → transport verbatim. This still fires for
///     transports like `signal` whose handle is a Signal-username
///     (no `+`), letting plugins that resolve `signal` directly
///     match.
pub fn resolver_kind_for(transport: &str, handle: &str) -> String {
    if looks_like_e164(handle) {
        return "phone".to_owned();
    }
    if looks_like_email(handle) {
        return "email".to_owned();
    }
    transport.to_owned()
}

/// E.164: leading `+`, then 7-15 digits. Per ITU-T E.164 the
/// total digit count is at most 15. Spaces/dashes inside aren't
/// canonical E.164 but we accept them for the resolver-kind check
/// since operators paste freely.
fn looks_like_e164(handle: &str) -> bool {
    if !handle.starts_with('+') {
        return false;
    }
    let digit_count = handle.chars().filter(|c| c.is_ascii_digit()).count();
    (7..=15).contains(&digit_count)
}

/// Loose email check: one `@`, at least one char on each side, and
/// the right-hand side contains a `.`. Doesn't claim RFC 5322
/// conformance — we just want to disambiguate from phone / handle.
fn looks_like_email(handle: &str) -> bool {
    let Some((local, domain)) = handle.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
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

// ---------------------------------------------------------------------------
// Reconcile: merge stale UnknownPending rows into higher-trust principals
// that own the same identifier.
// ---------------------------------------------------------------------------

/// What [`reconcile_against_my_identities`] did, for the operator's
/// audit log + the SPA's "Settings → My identities" save toast.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Stale UnknownPending principals merged away. Each entry is
    /// `(stale_pid, target_pid)`.
    pub merged: Vec<(String, String)>,
    /// Bindings re-pointed to the canonical principal_group.
    pub bindings_repointed: usize,
    /// Conversations re-bound + (when phased into
    /// awaiting_trust_decision) flipped back to idle.
    pub conversations_repointed: usize,
}

/// Walk the `principals` table and merge every UnknownPending row
/// whose identifiers are also present on a higher-trust principal
/// (typically the Controller via the operator's "My identities"
/// mapping).
///
/// This exists because identifier resolution is point-in-time:
/// when the operator adds `signal:+1...` to My identities AFTER
/// the inbound consumer has already minted an UnknownPending row
/// for that handle, the inbound binding still points at the stale
/// row. The next message would by-id-hit the stale row before the
/// helper's by-identifier check could rebind it. Reconcile fixes
/// the historical state in-place.
///
/// Steps per merge candidate:
///   1. Repoint every transport binding from `stale.principal_group`
///      to `target.principal_group`. Re-resolves the target group
///      with the controller's membership when the target is the
///      Controller (so the conversation lands as ControllerDM).
///   2. Repoint every conversation bound to the stale group at the
///      target group. Conversations stuck in
///      `awaiting_trust_decision` are flipped to `idle` so the
///      operator isn't stranded staring at a never-resolving
///      approval.
///   3. Delete the stale principal_group + its membership rows.
///   4. Delete the stale principal row.
///
/// The walk is intentionally read-modify-write: we don't take a
/// global lock, and a concurrent inbound writer could theoretically
/// race our rebind. In practice reconcile runs at boot or at the
/// instant the operator adds an identifier, both of which are
/// quiescent windows for that specific handle. A second reconcile
/// pass would clean up anything we missed.
pub fn reconcile_against_my_identities(
    db: &execlaw_core::db::Database,
) -> Result<ReconcileReport, DbError> {
    let principals = PrincipalStore::new(db);
    let pg_store = PrincipalGroupStore::new(db);
    let bindings = TransportBindingStore::new(db);
    let now = Utc::now().timestamp();

    let all = principals.list_all()?;

    // Build a quick lookup: for every identifier, which principals
    // claim it? Identifiers used by exactly one principal are
    // skipped; only the duplicates are merge candidates.
    let mut by_ident: std::collections::HashMap<Identifier, Vec<Principal>> =
        std::collections::HashMap::new();
    for p in &all {
        for ident in &p.identifiers {
            by_ident.entry(ident.clone()).or_default().push(p.clone());
        }
    }

    let mut report = ReconcileReport::default();
    // Track stale principals we've already merged so a principal
    // claiming multiple identifiers (each duplicated) doesn't get
    // processed twice.
    let mut already_merged: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (ident, claimants) in &by_ident {
        if claimants.len() < 2 {
            continue;
        }
        // Pick the canonical winner: highest trust rank wins, with
        // Controller pinned at the top. Ties break on first-seen
        // (oldest principal — typically the explicit "My identities"
        // controller row, since the UnknownPending row was minted
        // after the controller registered the handle).
        let mut sorted = claimants.clone();
        sorted.sort_by(|a, b| {
            trust_rank_for(&b.trust_level)
                .cmp(&trust_rank_for(&a.trust_level))
                .then_with(|| a.first_seen.cmp(&b.first_seen))
        });
        let target = &sorted[0];
        // Skip when the highest-trust claimant is itself
        // UnknownPending — there's no canonical winner to merge
        // toward, so leave them alone (operator can manually
        // resolve if it ever happens).
        if matches!(target.trust_level, CoreTrustLevel::UnknownPending { .. }) {
            continue;
        }

        for stale in sorted.iter().skip(1) {
            if !matches!(stale.trust_level, CoreTrustLevel::UnknownPending { .. }) {
                // Only merge AWAY from UnknownPending. A duplicate
                // where both sides are non-UnknownPending is an
                // operator data-entry mistake — surface (eventually)
                // rather than silently rebind.
                continue;
            }
            if already_merged.contains(stale.id.as_str()) {
                continue;
            }
            let stale_pid = stale.id.as_str().to_owned();
            let target_pid = target.id.as_str().to_owned();
            tracing::info!(
                target: "principal_admit::reconcile",
                identifier = ?ident,
                stale_pid = %stale_pid,
                target_pid = %target_pid,
                "merging stale UnknownPending principal into canonical claimant",
            );

            // Find every principal_group that has the stale
            // principal as a member. Each gets retired. (There
            // should be exactly one — the singleton group minted
            // at first-contact — but the helper handles the
            // multi-group case defensively.)
            let stale_groups = list_groups_for_member(db, &stale.id)?;

            for stale_group_id in &stale_groups {
                // Resolve / mint the canonical target group. The
                // group key MUST carry the SAME channel the stale
                // group was minted on — otherwise we mint a fresh
                // group under a different channel and leave the
                // original orphaned, breaking inbound routing for
                // that principal on the original transport.
                //
                // Read the channel from the stale group itself
                // rather than hardcoding (which previously baked
                // "signal" in and silently broke whatsapp / sms /
                // slack reconciliation as soon as those transports
                // shipped).
                let stale_channel = pg_store
                    .get(stale_group_id)?
                    .map(|g| g.channel)
                    .unwrap_or_else(|| {
                        // Defensive: stale group somehow gone from
                        // the store between list and resolve. Default
                        // to the resolver_kind we picked for the
                        // identifier — not perfect, but better than
                        // hardcoded "signal".
                        stale
                            .identifiers
                            .first()
                            .map(|i| i.transport.clone())
                            .unwrap_or_default()
                    });
                let is_controller = matches!(target.trust_level, CoreTrustLevel::Controller);
                let target_group = pg_store.resolve(
                    &execlaw_core::principal_groups::GroupKey {
                        channel: &stale_channel,
                        native_group_id: None,
                        principals: &[target.id.clone()],
                        includes_controller: is_controller,
                    },
                    now,
                )?;
                if target_group.group_id == *stale_group_id {
                    continue;
                }

                // Step 1: repoint every binding pointed at the stale
                // group → the target group.
                let bound = bindings.bindings_for_group_any_channel(stale_group_id)?;
                for b in &bound {
                    bindings.repoint_binding(
                        &b.channel,
                        &b.foreign_id,
                        &target_group.group_id,
                        now,
                    )?;
                    report.bindings_repointed += 1;
                }

                // Step 2: repoint every conversation bound to the
                // stale group → the target group, and unstick the
                // ones parked in awaiting_trust_decision.
                let convs = list_conversations_for_group(db, stale_group_id)?;
                for cid in &convs {
                    pg_store.bind_conversation(cid, &target_group.group_id)?;
                    flip_awaiting_trust_to_idle(db, cid, target.trust_level.class_tag(), now)?;
                    report.conversations_repointed += 1;
                }

                // Step 3: drop the stale group + its members. We
                // delete after the rebinds so a crash mid-loop
                // leaves a recoverable state (next reconcile
                // re-finds the same merge candidate).
                drop_group_members(db, stale_group_id)?;
                let _ = pg_store.delete(stale_group_id);
            }

            // Step 4: delete the stale principal row itself.
            let _ = principals.delete(&stale.id);
            already_merged.insert(stale_pid.clone());
            report.merged.push((stale_pid, target_pid));
        }
    }

    Ok(report)
}

fn trust_rank_for(t: &CoreTrustLevel) -> u32 {
    match t {
        CoreTrustLevel::Controller => 100,
        CoreTrustLevel::Delegated { .. } => 80,
        CoreTrustLevel::KnownTrusted { .. } => 60,
        CoreTrustLevel::KnownLimited { .. } => 40,
        CoreTrustLevel::Blocked { .. } => 20, // explicit decisions outrank UnknownPending
        CoreTrustLevel::UnknownPending { .. } => 1,
    }
}

fn list_groups_for_member(
    db: &execlaw_core::db::Database,
    pid: &PrincipalId,
) -> Result<Vec<String>, DbError> {
    db.with_conn(|c| {
        let mut stmt = c.prepare_cached(
            "SELECT group_id FROM state_principal_group_members WHERE principal_id = ?1",
        )?;
        let rows = stmt.query_map(params![pid.as_str()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

fn list_conversations_for_group(
    db: &execlaw_core::db::Database,
    group_id: &str,
) -> Result<Vec<String>, DbError> {
    db.with_conn(|c| {
        let mut stmt = c.prepare_cached(
            "SELECT conversation_id FROM state_conversations WHERE principal_group_id = ?1",
        )?;
        let rows = stmt.query_map(params![group_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

/// Flip a conversation's `phase` from `awaiting_trust_decision` to
/// `idle` and refresh its `trust_class` to whatever the merge
/// target's trust class is. Other phases are left alone — only the
/// stuck-on-cold-contact case needs rescuing.
fn flip_awaiting_trust_to_idle(
    db: &execlaw_core::db::Database,
    conversation_id: &str,
    new_trust_class: &str,
    now: i64,
) -> Result<(), DbError> {
    db.with_conn(|c| {
        c.execute(
            "UPDATE state_conversations \
             SET phase = CASE phase \
                            WHEN 'awaiting_trust_decision' THEN 'idle' \
                            ELSE phase \
                         END, \
                 trust_class = ?2, \
                 last_activity_at = ?3 \
             WHERE conversation_id = ?1",
            params![conversation_id, new_trust_class, now],
        )?;
        Ok(())
    })
}

fn drop_group_members(db: &execlaw_core::db::Database, group_id: &str) -> Result<(), DbError> {
    db.with_conn(|c| {
        c.execute(
            "DELETE FROM state_principal_group_members WHERE group_id = ?1",
            params![group_id],
        )?;
        Ok(())
    })
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

    #[test]
    fn resolver_kind_works_for_runtime_installed_phone_transports() {
        // Critical: the function shouldn't be hardcoded against a
        // small list of channels. Any future phone-shape transport
        // (imessage, voice, telnyx_sms, …) must auto-resolve to
        // "phone" purely from the handle's E.164 shape.
        assert_eq!(resolver_kind_for("imessage", "+14165550100"), "phone");
        assert_eq!(resolver_kind_for("voice", "+442071234567"), "phone");
        assert_eq!(resolver_kind_for("telnyx_sms", "+12345678"), "phone");
        // And non-phone transports still pass through.
        assert_eq!(resolver_kind_for("matrix", "@alice:example.com"), "matrix");
        assert_eq!(resolver_kind_for("xmpp", "alice@chat.example"), "email");
    }

    #[test]
    fn looks_like_e164_basic_cases() {
        assert!(looks_like_e164("+14165550100"));
        assert!(looks_like_e164("+442071234567"));
        assert!(looks_like_e164("+1 416-555-0100")); // pasted-with-formatting
        assert!(!looks_like_e164("14165550100")); // missing leading +
        assert!(!looks_like_e164("+12345")); // too short (5 < 7)
        assert!(!looks_like_e164(&format!("+{}", "1".repeat(16)))); // too long (16 > 15)
        assert!(!looks_like_e164(""));
        assert!(!looks_like_e164("+abc"));
    }

    #[test]
    fn looks_like_email_basic_cases() {
        assert!(looks_like_email("alice@example.com"));
        assert!(looks_like_email("a@b.c"));
        assert!(!looks_like_email("alice")); // no @
        assert!(!looks_like_email("@example.com")); // empty local
        assert!(!looks_like_email("alice@")); // empty domain
        assert!(!looks_like_email("alice@local")); // no TLD
        assert!(!looks_like_email("alice@.com")); // domain starts with dot
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

    #[test]
    fn reconcile_merges_stale_unknown_pending_into_controller() {
        // Replays the user's exact bug: an inbound Signal message
        // arrived BEFORE the controller registered the matching
        // "My identities" entry. The route_inbound_message path
        // minted `pri_signal_+1...` as UnknownPending and parked
        // its conversation in awaiting_trust_decision. After the
        // controller adds `signal:+1...`, reconcile must merge the
        // stale row and unstick the conversation.
        use execlaw_core::conversation::{
            ConversationKind, ConversationRow, ConversationStore, Phase,
        };
        use execlaw_core::ids::ConversationId;
        use execlaw_core::principal::Identifier;
        use execlaw_core::principal_groups::{GroupKey, PrincipalGroupStore};
        use execlaw_core::transport_bindings::TransportBindingStore;

        let db = fresh_db();
        let principals = PrincipalStore::new(&db);
        let pg_store = PrincipalGroupStore::new(&db);
        let bindings = TransportBindingStore::new(&db);
        let conversations = ConversationStore::new(&db);
        let now = 100;

        // Fixture: stale UnknownPending principal + group + binding
        // + conversation (the cold-contact-arrived state).
        let stale_pid = PrincipalId::from("pri_signal_+15551234567");
        let stale_principal = Principal {
            id: stale_pid.clone(),
            identifiers: vec![Identifier {
                transport: "signal".into(),
                handle: "+15551234567".into(),
            }],
            trust_level: CoreTrustLevel::UnknownPending {
                first_seen: now,
                notification_event_seq: None,
            },
            resolved_by: Vec::new(),
            metadata: serde_json::json!({}),
            first_seen: now,
            last_seen: Some(now),
            controller_notes: None,
        };
        principals.upsert(&stale_principal).unwrap();
        let stale_group = pg_store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &[stale_pid.clone()],
                    includes_controller: false,
                },
                now,
            )
            .unwrap();
        bindings
            .insert_binding("signal", "+15551234567", &stale_group.group_id, false, now)
            .unwrap();
        let cid = ConversationId::from_string("conv-stuck");
        let row = ConversationRow {
            conversation_id: cid.clone(),
            kind: ConversationKind::ControllerDM,
            last_seq: execlaw_core::ids::EventSeq(0),
            phase: Phase::AwaitingTrustDecision,
            controller_id: None,
            trust_class: "UnknownPending".into(),
            snapshot_blob: None,
            snapshot_seq: None,
            lease_owner: None,
            lease_expires: None,
            modality: execlaw_core::conversation::Modality::Text,
            display_name: None,
            display_name_source: "auto".into(),
            is_pinned: false,
            is_ephemeral: false,
            ephemeral_expires_at: None,
            last_activity_at: now,
        };
        conversations.upsert(&row).unwrap();
        pg_store
            .bind_conversation(cid.as_str(), &stale_group.group_id)
            .unwrap();

        // Operator action: register the same handle on the
        // controller principal — exactly what add_my_identifier
        // does.
        let controller_pid = PrincipalId::from("controller-x");
        let controller = Principal {
            id: controller_pid.clone(),
            identifiers: vec![Identifier {
                transport: "signal".into(),
                handle: "+15551234567".into(),
            }],
            trust_level: CoreTrustLevel::Controller,
            resolved_by: Vec::new(),
            metadata: serde_json::json!({}),
            first_seen: now - 1,
            last_seen: Some(now),
            controller_notes: None,
        };
        principals.upsert(&controller).unwrap();

        // Reconcile.
        let report = reconcile_against_my_identities(&db).unwrap();
        assert_eq!(report.merged.len(), 1);
        assert_eq!(report.merged[0].0, "pri_signal_+15551234567");
        assert_eq!(report.merged[0].1, "controller-x");
        assert!(report.bindings_repointed >= 1);
        assert!(report.conversations_repointed >= 1);

        // Stale principal is gone.
        assert!(principals.get(&stale_pid).unwrap().is_none());
        // Binding now points at a controller-owned group.
        let target_pg = pg_store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &[controller_pid.clone()],
                    includes_controller: true,
                },
                now,
            )
            .unwrap();
        let pg = bindings
            .lookup_principal_group("signal", "+15551234567")
            .unwrap();
        assert_eq!(pg, Some(target_pg.group_id.clone()));
        // Conversation is unstuck (phase=idle, trust_class=Controller).
        let updated = conversations.get(&cid).unwrap().unwrap();
        assert_eq!(updated.phase, Phase::Idle);
        assert_eq!(updated.trust_class, "Controller");
        // principal_group_id lives on the row in the schema but not
        // on the typed projection — query separately.
        let bound_pg = pg_store
            .principal_group_id_for(cid.as_str())
            .unwrap()
            .expect("conversation should be bound to a principal_group");
        assert_eq!(bound_pg, target_pg.group_id);

        // A second reconcile pass is a no-op — the stale row is
        // gone, nothing to merge.
        let report2 = reconcile_against_my_identities(&db).unwrap();
        assert!(report2.merged.is_empty());
    }

    #[test]
    fn reconcile_skips_unique_identifiers() {
        // No duplicate identifier → no merge candidate → empty
        // report. Cheap path that matters because reconcile runs
        // on every boot.
        let db = fresh_db();
        let principals = PrincipalStore::new(&db);
        let p = Principal {
            id: PrincipalId::from("only-one"),
            identifiers: vec![Identifier {
                transport: "signal".into(),
                handle: "+15550000001".into(),
            }],
            trust_level: CoreTrustLevel::UnknownPending {
                first_seen: 0,
                notification_event_seq: None,
            },
            resolved_by: Vec::new(),
            metadata: serde_json::json!({}),
            first_seen: 0,
            last_seen: None,
            controller_notes: None,
        };
        principals.upsert(&p).unwrap();
        let report = reconcile_against_my_identities(&db).unwrap();
        assert!(report.merged.is_empty());
        assert_eq!(report.bindings_repointed, 0);
        assert_eq!(report.conversations_repointed, 0);
        // Original principal is still there.
        assert!(principals.get(&p.id).unwrap().is_some());
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
        assert!(matches!(
            got.trust_level,
            CoreTrustLevel::UnknownPending { .. }
        ));
        assert_eq!(flat, TrustLevel::UnknownPending);
        // Identifier was written so the next inbound finds it via
        // exact-id hit (step 1).
        assert_eq!(got.identifiers.len(), 1);
        assert_eq!(got.identifiers[0].transport, "signal");
        assert_eq!(got.identifiers[0].handle, "+19998887777");
    }
}
