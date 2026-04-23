//! Rule of Two (§2.2 axiom #9, §2.6, §7.3).
//!
//! "Per turn: `untrusted_input_in_turn + accesses_sensitive_data +
//! produces_external_effect ≤ 2`. The third property forces HITL."
//!
//! This module is *pure*: the decision is an arithmetic check over booleans.
//! Whether the decision leads to approval dispatch, a denial event, etc.,
//! is up to the caller.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleOfTwoInput {
    pub untrusted_input_in_turn: bool,
    pub accesses_sensitive_data: bool,
    pub produces_external_effect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleOfTwoVerdict {
    /// Fewer than three of the three properties are true; turn may proceed.
    Allow,
    /// All three properties are true; turn must go through HITL.
    RequireApproval,
}

pub fn rule_of_two_verdict(i: RuleOfTwoInput) -> RuleOfTwoVerdict {
    let count = i.untrusted_input_in_turn as u32
        + i.accesses_sensitive_data as u32
        + i.produces_external_effect as u32;
    if count >= 3 {
        RuleOfTwoVerdict::RequireApproval
    } else {
        RuleOfTwoVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_three_true_requires_approval() {
        let v = rule_of_two_verdict(RuleOfTwoInput {
            untrusted_input_in_turn: true,
            accesses_sensitive_data: true,
            produces_external_effect: true,
        });
        assert_eq!(v, RuleOfTwoVerdict::RequireApproval);
    }

    #[test]
    fn two_true_allowed() {
        let v = rule_of_two_verdict(RuleOfTwoInput {
            untrusted_input_in_turn: true,
            accesses_sensitive_data: true,
            produces_external_effect: false,
        });
        assert_eq!(v, RuleOfTwoVerdict::Allow);
    }

    #[test]
    fn zero_true_allowed() {
        let v = rule_of_two_verdict(RuleOfTwoInput {
            untrusted_input_in_turn: false,
            accesses_sensitive_data: false,
            produces_external_effect: false,
        });
        assert_eq!(v, RuleOfTwoVerdict::Allow);
    }
}
