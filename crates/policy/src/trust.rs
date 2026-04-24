//! Trust ladder → capability set (§2.6, §7.3).
//!
//! Takes the principal's trust level plus the turn's properties
//! (sender/addressee/effective trust) and computes:
//!
//! - the capability set the runner should be granted
//! - whether planner/executor split is required
//! - whether Rule-of-Two should promote the turn to `awaiting_approval`
//! - the maximum tool inventory the runner should see (latency filter)
//!
//! This is pure policy logic — no I/O. The caller (turn executor)
//! applies the decisions against the event log and the inference
//! request.

use serde::{Deserialize, Serialize};

/// The trust levels from `crates/core/src/principal.rs` restated as a
/// small flat enum — policy code shouldn't care about the rich
/// `Delegated { by, scope, expires_at }` data, just the rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum TrustLevel {
    /// The operator / admin. Full capabilities.
    Controller,
    /// Time-bounded, capability-scoped grant from the controller.
    Delegated,
    /// Identity-provider matched + controller-approved.
    KnownTrusted,
    /// Identity-matched but topic/tool-limited.
    KnownLimited,
    /// First-time contact; conversation parks until the controller
    /// responds (§2.14). `TurnEvaluator::evaluate` will not grant any
    /// capability here — the caller should not even reach this code
    /// path for a `UnknownPending` sender.
    UnknownPending,
    /// Controller explicitly rejected this principal (universal `Blocked`
    /// state — may be an unknown contact who was refused or a known
    /// principal who was revoked).
    Blocked,
}

impl TrustLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Controller => "Controller",
            Self::Delegated => "Delegated",
            Self::KnownTrusted => "KnownTrusted",
            Self::KnownLimited => "KnownLimited",
            Self::UnknownPending => "UnknownPending",
            Self::Blocked => "Blocked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Controller" => Some(Self::Controller),
            "Delegated" => Some(Self::Delegated),
            "KnownTrusted" => Some(Self::KnownTrusted),
            "KnownLimited" => Some(Self::KnownLimited),
            "UnknownPending" => Some(Self::UnknownPending),
            "Blocked" => Some(Self::Blocked),
            _ => None,
        }
    }

    /// Numeric rank — higher = more trusted. Used for `<=` comparisons.
    pub fn rank(self) -> u8 {
        match self {
            Self::Controller => 5,
            Self::Delegated => 4,
            Self::KnownTrusted => 3,
            Self::KnownLimited => 2,
            Self::UnknownPending => 1,
            Self::Blocked => 0,
        }
    }
}

/// Allowed tool-latency classes at a given trust level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LatencyBand {
    /// All latency classes exposed (default for text + voice controllers).
    Any,
    /// `low` only — used for voice turns to keep the conversation snappy
    /// (§2.13.4); also used for `ExternalWithOutsider` to tighten
    /// surface area.
    LowOnly,
}

/// Inputs to the per-turn policy evaluation.
#[derive(Debug, Clone, Copy)]
pub struct TurnPolicyInput {
    /// The effective trust level for this conversation — min across
    /// all principals who can read the agent's reply (§2.6).
    pub effective_trust: TrustLevel,
    /// Trust level of the principal whose message triggered this turn.
    pub sender_trust: TrustLevel,
    /// Is this a voice turn (§2.13)? Tightens latency band.
    pub voice: bool,
    /// Does this turn plan to access sensitive data?
    pub accesses_sensitive_data: bool,
    /// Does this turn plan to produce an external side-effect?
    pub produces_external_effect: bool,
}

/// Outputs of policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPolicyDecision {
    /// Should the turn even run, or be rejected at ingress?
    pub drop_turn: bool,
    /// Should the turn park in `awaiting_approval` before the model is
    /// called (Rule-of-Two or blocked-read attempt)?
    pub require_approval: bool,
    /// Planner/executor split required for this turn (§2.5)?
    pub planner_executor: bool,
    /// Apply STT-style spotlighting to any untrusted content (§7.4)?
    pub spotlighting: bool,
    /// Which tool latencies are exposed to the model.
    pub latency_band: LatencyBand,
    /// Capability strings included in the capability token (§7.2).
    ///
    /// Backed by a `&'static` slice — the capability tiers are
    /// discrete (one per trust level), so we point at a compile-time
    /// table rather than allocating a fresh `Vec<String>` per call.
    /// This saved ~290ns per call (§0 axiom #14 benchmark).
    pub capability_set: &'static [&'static str],
}

// Discrete capability tiers. Each trust level maps to exactly one of
// these slices — no composition, no per-call allocation.
const CAPS_NONE: &[&str] = &[];
const CAPS_WILDCARD: &[&str] = &["*"];
const CAPS_KNOWN_LIMITED: &[&str] = &["messaging.reply_current_transport"];
const CAPS_KNOWN_TRUSTED: &[&str] = &[
    "messaging.reply_current_transport",
    "memory.read",
    "memory.write",
    "tools.safe",
];
const CAPS_DELEGATED: &[&str] = &[
    "messaging.reply_current_transport",
    "memory.read",
    "memory.write",
    "tools.safe",
    "tools.medium",
    "controller.ask",
];

/// The main evaluator. Pure; returns its decision, never mutates state.
pub fn evaluate_turn(input: TurnPolicyInput) -> TurnPolicyDecision {
    // Blocked senders: message never reaches the agent.
    if input.sender_trust == TrustLevel::Blocked {
        return TurnPolicyDecision {
            drop_turn: true,
            require_approval: false,
            planner_executor: false,
            spotlighting: false,
            latency_band: LatencyBand::LowOnly,
            capability_set: CAPS_NONE,
        };
    }

    // UnknownPending senders: conversation parks until the controller
    // admits or rejects them. Returning `require_approval=true` gets
    // the caller to trigger the cold-contact notification flow.
    if input.sender_trust == TrustLevel::UnknownPending {
        return TurnPolicyDecision {
            drop_turn: false,
            require_approval: true,
            planner_executor: true,
            spotlighting: true,
            latency_band: LatencyBand::LowOnly,
            capability_set: CAPS_NONE,
        };
    }

    // At this point the sender is at least KnownLimited. Pick the
    // discrete capability tier that matches sender_trust.
    let capability_set: &'static [&'static str] = match input.sender_trust {
        TrustLevel::Controller => CAPS_WILDCARD,
        TrustLevel::Delegated => CAPS_DELEGATED,
        TrustLevel::KnownTrusted => CAPS_KNOWN_TRUSTED,
        TrustLevel::KnownLimited => CAPS_KNOWN_LIMITED,
        // Unreachable — UnknownPending + Blocked handled above.
        _ => CAPS_NONE,
    };

    // Planner/executor split for anything less than full trust.
    let planner_executor = input.effective_trust.rank() < TrustLevel::KnownTrusted.rank();

    // Spotlighting for untrusted content.
    let spotlighting = input.effective_trust.rank() < TrustLevel::KnownTrusted.rank();

    // Latency band: voice turns are always low-only; outsider/limited
    // turns are also low-only to shrink surface area.
    let latency_band =
        if input.voice || input.effective_trust.rank() <= TrustLevel::KnownLimited.rank() {
            LatencyBand::LowOnly
        } else {
            LatencyBand::Any
        };

    // Rule of Two: untrusted input + sensitive data + external effect ≤ 2.
    // "Untrusted input" is any effective trust below KnownTrusted.
    let untrusted = input.effective_trust.rank() < TrustLevel::KnownTrusted.rank();
    let count = (untrusted as u8)
        + (input.accesses_sensitive_data as u8)
        + (input.produces_external_effect as u8);
    let require_approval = count >= 3;

    TurnPolicyDecision {
        drop_turn: false,
        require_approval,
        planner_executor,
        spotlighting,
        latency_band,
        capability_set,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        effective: TrustLevel,
        sender: TrustLevel,
        sensitive: bool,
        external: bool,
    ) -> TurnPolicyInput {
        TurnPolicyInput {
            effective_trust: effective,
            sender_trust: sender,
            voice: false,
            accesses_sensitive_data: sensitive,
            produces_external_effect: external,
        }
    }

    #[test]
    fn controller_gets_wildcard_caps() {
        let d = evaluate_turn(input(
            TrustLevel::Controller,
            TrustLevel::Controller,
            false,
            false,
        ));
        assert_eq!(d.capability_set, &["*"]);
        assert!(!d.planner_executor);
        assert!(!d.spotlighting);
        assert_eq!(d.latency_band, LatencyBand::Any);
    }

    #[test]
    fn blocked_drops_turn() {
        let d = evaluate_turn(input(
            TrustLevel::Controller,
            TrustLevel::Blocked,
            false,
            false,
        ));
        assert!(d.drop_turn);
        assert!(d.capability_set.is_empty());
    }

    #[test]
    fn unknown_pending_requires_approval_before_agent_runs() {
        let d = evaluate_turn(input(
            TrustLevel::UnknownPending,
            TrustLevel::UnknownPending,
            false,
            false,
        ));
        assert!(!d.drop_turn);
        assert!(d.require_approval);
        assert!(d.planner_executor);
        assert!(d.spotlighting);
    }

    #[test]
    fn outsider_gets_planner_executor_split_and_spotlighting() {
        let d = evaluate_turn(input(
            TrustLevel::KnownLimited,
            TrustLevel::KnownLimited,
            false,
            false,
        ));
        assert!(d.planner_executor);
        assert!(d.spotlighting);
        assert_eq!(d.latency_band, LatencyBand::LowOnly);
    }

    #[test]
    fn known_trusted_gets_no_split_and_full_latency() {
        let d = evaluate_turn(input(
            TrustLevel::KnownTrusted,
            TrustLevel::KnownTrusted,
            false,
            false,
        ));
        assert!(!d.planner_executor);
        assert!(!d.spotlighting);
        assert_eq!(d.latency_band, LatencyBand::Any);
    }

    #[test]
    fn rule_of_two_triggers_with_all_three_properties_for_outsider() {
        let d = evaluate_turn(input(
            TrustLevel::KnownLimited,
            TrustLevel::KnownLimited,
            true,
            true,
        ));
        assert!(d.require_approval);
    }

    #[test]
    fn rule_of_two_allows_two_properties() {
        let d = evaluate_turn(input(
            TrustLevel::KnownLimited,
            TrustLevel::KnownLimited,
            true,
            false,
        ));
        assert!(!d.require_approval);
    }

    #[test]
    fn voice_always_low_latency_even_for_controller() {
        let mut inp = input(TrustLevel::Controller, TrustLevel::Controller, false, false);
        inp.voice = true;
        let d = evaluate_turn(inp);
        assert_eq!(d.latency_band, LatencyBand::LowOnly);
    }

    // ---- Adversarial / capability-leak tests -------------------------------

    /// A KnownLimited sender must NOT see `*`, `tools.medium`, or
    /// `controller.ask` in its capability set — those are reserved for
    /// Delegated+ and Controller respectively. This is the test that
    /// would fail if someone accidentally relaxed the rank check.
    #[test]
    fn known_limited_sender_does_not_leak_higher_trust_caps() {
        let d = evaluate_turn(input(
            TrustLevel::KnownLimited,
            TrustLevel::KnownLimited,
            false,
            false,
        ));
        assert!(!d.capability_set.contains(&"*"));
        assert!(!d.capability_set.contains(&"tools.medium"));
        assert!(!d.capability_set.contains(&"controller.ask"));
        assert!(!d.capability_set.contains(&"memory.read"));
        assert!(!d.capability_set.contains(&"memory.write"));
        assert!(
            d.capability_set.contains(&"messaging.reply_current_transport"),
            "even KnownLimited must be able to reply on current transport"
        );
    }

    /// A KnownTrusted sender gets memory + safe tools, but still must
    /// NOT see controller wildcards or medium-risk tools.
    #[test]
    fn known_trusted_sender_gets_memory_but_not_medium_tools() {
        let d = evaluate_turn(input(
            TrustLevel::KnownTrusted,
            TrustLevel::KnownTrusted,
            false,
            false,
        ));
        assert!(d.capability_set.contains(&"memory.read"));
        assert!(d.capability_set.contains(&"memory.write"));
        assert!(d.capability_set.contains(&"tools.safe"));
        assert!(!d.capability_set.contains(&"tools.medium"));
        assert!(!d.capability_set.contains(&"*"));
    }

    /// Delegated gets medium tools and controller.ask — but still not `*`.
    #[test]
    fn delegated_sender_gets_medium_but_not_wildcard() {
        let d = evaluate_turn(input(
            TrustLevel::Delegated,
            TrustLevel::Delegated,
            false,
            false,
        ));
        assert!(d.capability_set.contains(&"tools.medium"));
        assert!(d.capability_set.contains(&"controller.ask"));
        assert!(!d.capability_set.contains(&"*"));
    }

    /// Voice + KnownTrusted must still produce LowOnly latency band —
    /// voice tightening wins over the effective-trust check.
    #[test]
    fn voice_forces_low_latency_for_known_trusted() {
        let mut inp = input(
            TrustLevel::KnownTrusted,
            TrustLevel::KnownTrusted,
            false,
            false,
        );
        inp.voice = true;
        let d = evaluate_turn(inp);
        assert_eq!(d.latency_band, LatencyBand::LowOnly);
    }

    /// Rank ordering is the load-bearing primitive for capability
    /// cascading — lock it down explicitly so a reorder in the enum
    /// can't silently shift who gets what.
    #[test]
    fn trust_rank_order_is_stable() {
        assert!(TrustLevel::Controller.rank() > TrustLevel::Delegated.rank());
        assert!(TrustLevel::Delegated.rank() > TrustLevel::KnownTrusted.rank());
        assert!(TrustLevel::KnownTrusted.rank() > TrustLevel::KnownLimited.rank());
        assert!(TrustLevel::KnownLimited.rank() > TrustLevel::UnknownPending.rank());
        assert!(TrustLevel::UnknownPending.rank() > TrustLevel::Blocked.rank());
    }

    /// Parse accepts only the exact class tags; anything else must
    /// return None so unknown trust classes don't silently default.
    #[test]
    fn trust_parse_rejects_unknown_tags() {
        assert_eq!(TrustLevel::parse("Controller"), Some(TrustLevel::Controller));
        assert_eq!(TrustLevel::parse("Blocked"), Some(TrustLevel::Blocked));
        assert_eq!(TrustLevel::parse("Admin"), None);
        assert_eq!(TrustLevel::parse(""), None);
        assert_eq!(TrustLevel::parse("controller"), None, "case-sensitive");
    }

    /// Rule of Two must count "untrusted" as effective_trust BELOW
    /// KnownTrusted. A KnownTrusted effective trust should NOT count
    /// as untrusted, even with both other properties true.
    #[test]
    fn rule_of_two_does_not_fire_for_trusted_effective_trust() {
        let d = evaluate_turn(input(
            TrustLevel::KnownTrusted,
            TrustLevel::KnownTrusted,
            true,
            true,
        ));
        assert!(
            !d.require_approval,
            "KnownTrusted is not 'untrusted input' — only 2 of 3 properties present"
        );
    }
}
