//! Sideband approval flow (§2.11, §2.14).
//!
//! When the agent needs controller approval — a Rule-of-Two trigger,
//! a cold-contact admittance request, a sensitive tool invocation —
//! the control plane issues a **signed approval request** carrying a
//! one-time token, dispatches it over a *different* transport than
//! the one that triggered it, and parks the conversation until the
//! controller's signed response arrives.
//!
//! Key properties:
//!
//! - The approval token is Ed25519-signed by the server (same key the
//!   capability tokens use), so transports can't forge approvals.
//! - The token is single-use: the server records the token's `jti` in
//!   `state_inbox` on first consumption; replays are rejected.
//! - The notification goes over a **different transport** from the one
//!   that introduced the untrusted content — so an attacker controlling
//!   Signal can't approve their own request via Signal.

use serde::{Deserialize, Serialize};

/// Why the agent is asking the controller for approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    /// Cold-contact admittance: principal is UnknownPending; controller
    /// decides `Trust` / `TrustLimited` / `Block` / `IgnoreOnce`.
    ColdContact,
    /// Rule of Two tripped: all three properties true, tool call held.
    RuleOfTwoBreach,
    /// A specific tool call marked `approval.required` in the policy.
    SensitiveToolCall,
    /// Agent explicitly asked the controller for guidance via
    /// `ask_controller(question, urgency)`.
    AskController,
    /// Anomaly tripwire fired (burst tool calls / policy denials).
    AnomalyTripwire,
}

/// The controller's response verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalVerb {
    Approve,
    /// `Edit` — controller tweaked the proposed tool-call args before
    /// approving. The edited args travel in the response body.
    Edit,
    Reject,
    /// For cold-contact: admit this principal as KnownTrusted.
    Trust,
    /// For cold-contact: admit as KnownLimited with named topics.
    TrustLimited,
    /// For cold-contact: block this principal universally.
    Block,
    /// For cold-contact: drop this message, re-prompt on the next one.
    IgnoreOnce,
}

/// The payload signed into the approval token (JWT claims).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalClaims {
    pub iss: String,
    /// Approval-request id (matches `state_events[...].approval.id`).
    pub jti: String,
    /// Conversation the approval applies to.
    pub conversation_id: String,
    /// Why the controller is being asked.
    pub reason: ApprovalReason,
    /// Specific tool call this approval gates, if any.
    pub tool_call_id: Option<String>,
    /// When the request was minted.
    pub iat: i64,
    /// Hard expiry — after this, the token must be re-minted.
    pub exp: i64,
}

/// Pick a sideband transport to use for a notification. The caller
/// supplies the full list of *enabled* transports and the id of the
/// originating transport; this function picks the highest-priority
/// transport that is NOT the originator. Returns `None` if only the
/// originating transport is available (degenerate case — we fall back
/// to the UI notification dropdown).
pub fn pick_sideband_transport<'a>(
    enabled: &'a [&'a str],
    origin: &str,
    priority: &[&str],
) -> Option<&'a str> {
    let eligible: Vec<&str> = enabled.iter().copied().filter(|t| *t != origin).collect();
    if eligible.is_empty() {
        return None;
    }
    for wanted in priority {
        if eligible.contains(wanted) {
            return eligible.into_iter().find(|t| t == wanted);
        }
    }
    // No explicit priority hit; take the first eligible transport.
    eligible.first().copied()
}

/// Default sideband priority — Signal first (most operators have it),
/// then email, then web-ui (always-on).
pub fn default_sideband_priority() -> Vec<&'static str> {
    vec!["signal", "email", "webui"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_first_priority_transport_that_is_not_origin() {
        let enabled = ["signal", "email", "webui"];
        let priority = ["signal", "email", "webui"];
        let pick = pick_sideband_transport(&enabled, "signal", &priority);
        assert_eq!(pick, Some("email"));
    }

    #[test]
    fn falls_back_to_any_eligible_if_no_priority_match() {
        let enabled = ["matrix"];
        let priority = ["signal", "email"];
        let pick = pick_sideband_transport(&enabled, "other", &priority);
        assert_eq!(pick, Some("matrix"));
    }

    #[test]
    fn returns_none_when_only_origin_transport_available() {
        let enabled = ["signal"];
        let priority = ["signal", "email"];
        let pick = pick_sideband_transport(&enabled, "signal", &priority);
        assert_eq!(pick, None);
    }

    #[test]
    fn origin_filter_honors_exact_match() {
        let enabled = ["signal", "signal"];
        let priority = ["signal"];
        let pick = pick_sideband_transport(&enabled, "signal", &priority);
        assert_eq!(pick, None);
    }

    #[test]
    fn approval_claim_round_trips_json() {
        let claim = ApprovalClaims {
            iss: "execlaw".into(),
            jti: "appr-1".into(),
            conversation_id: "conv-1".into(),
            reason: ApprovalReason::ColdContact,
            tool_call_id: None,
            iat: 0,
            exp: 3600,
        };
        let s = serde_json::to_string(&claim).unwrap();
        assert!(s.contains("\"reason\":\"cold_contact\""));
        let back: ApprovalClaims = serde_json::from_str(&s).unwrap();
        assert_eq!(back.reason, ApprovalReason::ColdContact);
    }

    /// Every ApprovalVerb must round-trip through snake_case JSON — a
    /// regression here would silently break the sideband response path.
    #[test]
    fn approval_verb_round_trips_all_variants() {
        for v in [
            ApprovalVerb::Approve,
            ApprovalVerb::Edit,
            ApprovalVerb::Reject,
            ApprovalVerb::Trust,
            ApprovalVerb::TrustLimited,
            ApprovalVerb::Block,
            ApprovalVerb::IgnoreOnce,
        ] {
            let s = serde_json::to_string(&v).unwrap();
            let back: ApprovalVerb = serde_json::from_str(&s).unwrap();
            assert_eq!(back, v);
        }
        // Specific snake_case contract for TrustLimited and IgnoreOnce.
        assert_eq!(
            serde_json::to_string(&ApprovalVerb::TrustLimited).unwrap(),
            "\"trust_limited\""
        );
        assert_eq!(
            serde_json::to_string(&ApprovalVerb::IgnoreOnce).unwrap(),
            "\"ignore_once\""
        );
    }

    #[test]
    fn approval_reason_round_trips_all_variants() {
        for r in [
            ApprovalReason::ColdContact,
            ApprovalReason::RuleOfTwoBreach,
            ApprovalReason::SensitiveToolCall,
            ApprovalReason::AskController,
            ApprovalReason::AnomalyTripwire,
        ] {
            let s = serde_json::to_string(&r).unwrap();
            let back: ApprovalReason = serde_json::from_str(&s).unwrap();
            assert_eq!(back, r);
        }
    }

    /// Empty enabled list => None. Degenerate deployment, but exercised.
    #[test]
    fn returns_none_when_no_transports_enabled() {
        let pick = pick_sideband_transport(&[], "signal", &["signal", "email"]);
        assert_eq!(pick, None);
    }

    #[test]
    fn default_priority_order_is_signal_email_webui() {
        let p = default_sideband_priority();
        assert_eq!(p, vec!["signal", "email", "webui"]);
    }

    /// Sideband priority honors order: when both signal and email are
    /// enabled and the origin is webui, signal wins over email because
    /// it appears earlier in the priority list.
    #[test]
    fn priority_order_wins_over_enumeration_order() {
        let enabled = ["email", "signal"]; // enumeration is email-first
        let priority = ["signal", "email"]; // priority still says signal
        let pick = pick_sideband_transport(&enabled, "webui", &priority);
        assert_eq!(pick, Some("signal"));
    }
}
