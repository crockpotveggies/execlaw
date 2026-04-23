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
pub mod token;

pub use rule_of_two::{rule_of_two_verdict, RuleOfTwoInput, RuleOfTwoVerdict};
pub use token::{CapabilityTokenClaims, CapabilityTokenError};
