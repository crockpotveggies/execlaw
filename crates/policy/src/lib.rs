//! execlaw-policy
//!
//! Capability-token issuance (EdDSA-signed JWTs, §7.2), Rule-of-Two check,
//! and — later — the full rules-table evaluator (§7.3).
//!
//! Phase 0 ships the Rule-of-Two helper + token payload shape + input-guard
//! utilities (homoglyph folding, zero-width strip).

#![forbid(unsafe_code)]

pub mod input_guard;
pub mod rule_of_two;
pub mod sideband;
pub mod spotlighting;
pub mod token;
pub mod trust;

pub use rule_of_two::{RuleOfTwoInput, RuleOfTwoVerdict, rule_of_two_verdict};
pub use sideband::{
    ApprovalClaims, ApprovalReason, ApprovalVerb, default_sideband_priority,
    pick_sideband_transport,
};
pub use spotlighting::{Spotlight, should_spotlight};
pub use token::{CapabilityTokenClaims, CapabilityTokenError};
pub use trust::{LatencyBand, TrustLevel, TurnPolicyDecision, TurnPolicyInput, evaluate_turn};
