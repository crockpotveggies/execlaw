//! Barge-in + false-barge-in rescind (§2.13.3).
//!
//! When VAD detects user speech mid-TTS:
//!
//! 1. Fire `SystemFrame::UserStartedSpeaking` onto the system lane.
//! 2. Start a `rescind_delay_ms` timer (~120ms default).
//! 3. During the delay, pass STT output through a **backchannel
//!    detector**. If the delayed transcript is a known backchannel
//!    (`"mm-hmm"`, `"yeah"`, `"okay"`) AND the utterance ends within
//!    ~400ms, rescind the interrupt — the user is just acknowledging,
//!    not actually interrupting. The TTS keeps rolling.
//! 4. If the timer expires without a backchannel match OR the user
//!    keeps talking past the rescind window, commit the interrupt:
//!    halt TTS, cancel in-flight LLM tokens.
//!
//! This module is pure logic — no I/O, no timers. Callers drive the
//! timers; we just answer "given this transcript fragment and this
//! elapsed time, rescind or confirm?"

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BargeInDecision {
    /// Hold the interrupt; keep waiting.
    Wait,
    /// User was just backchanneling — resume TTS.
    Rescind,
    /// Real interrupt — halt TTS + cancel in-flight LLM.
    Confirm,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BargeInConfig {
    /// How long we wait for STT to resolve before deciding.
    pub rescind_delay_ms: u32,
    /// Max total utterance duration that still counts as a backchannel.
    pub max_backchannel_ms: u32,
}

impl Default for BargeInConfig {
    fn default() -> Self {
        Self {
            rescind_delay_ms: 120,
            max_backchannel_ms: 400,
        }
    }
}

/// Case-insensitive backchannel vocabulary. Conservative — only match
/// if the entire transcript fragment is one of these tokens.
pub fn is_backchannel(transcript: &str) -> bool {
    let normalized: String = transcript
        .trim()
        .trim_end_matches(['.', ',', '!', '?'])
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "mm-hmm"
            | "mmhmm"
            | "mhm"
            | "uh huh"
            | "uh-huh"
            | "yeah"
            | "yep"
            | "yup"
            | "ok"
            | "okay"
            | "right"
            | "sure"
            | "got it"
            | "i see"
            | "roger"
            | "cool"
            | "sounds good"
    )
}

/// Given the current state of a potential barge-in, decide whether to
/// rescind, confirm, or keep waiting.
pub fn decide(
    cfg: &BargeInConfig,
    elapsed_ms: u32,
    latest_transcript: &str,
    user_still_speaking: bool,
) -> BargeInDecision {
    if elapsed_ms < cfg.rescind_delay_ms {
        return BargeInDecision::Wait;
    }
    // Past the rescind window. If the user is still speaking AFTER the
    // backchannel cap, confirm the interrupt.
    if user_still_speaking && elapsed_ms > cfg.max_backchannel_ms {
        return BargeInDecision::Confirm;
    }
    if is_backchannel(latest_transcript) && elapsed_ms <= cfg.max_backchannel_ms {
        return BargeInDecision::Rescind;
    }
    if user_still_speaking {
        BargeInDecision::Wait
    } else {
        BargeInDecision::Confirm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_backchannels_recognized() {
        for v in ["mm-hmm", "Yeah", "okay.", "Right,", "Got it"] {
            assert!(is_backchannel(v), "{v:?} should be a backchannel");
        }
    }

    #[test]
    fn real_content_is_not_backchannel() {
        for v in ["hold on let me check", "cancel that", "what's the weather"] {
            assert!(!is_backchannel(v), "{v:?} should NOT be a backchannel");
        }
    }

    #[test]
    fn waits_within_rescind_delay() {
        let cfg = BargeInConfig::default();
        assert_eq!(decide(&cfg, 50, "mm-hmm", true), BargeInDecision::Wait);
    }

    #[test]
    fn rescinds_on_backchannel_within_cap() {
        let cfg = BargeInConfig::default();
        // 150ms elapsed, utterance is a backchannel, user stopped.
        assert_eq!(decide(&cfg, 150, "yeah", false), BargeInDecision::Rescind);
    }

    #[test]
    fn confirms_when_user_keeps_going_past_cap() {
        let cfg = BargeInConfig::default();
        assert_eq!(
            decide(&cfg, 500, "cancel that", true),
            BargeInDecision::Confirm
        );
    }

    #[test]
    fn confirms_on_non_backchannel_when_user_finished() {
        let cfg = BargeInConfig::default();
        assert_eq!(
            decide(&cfg, 250, "wait no", false),
            BargeInDecision::Confirm
        );
    }

    /// Defaults match the §2.13.3 spec numbers — drift guard.
    #[test]
    fn default_config_matches_spec_numbers() {
        let cfg = BargeInConfig::default();
        assert_eq!(cfg.rescind_delay_ms, 120);
        assert_eq!(cfg.max_backchannel_ms, 400);
    }

    /// Even if a backchannel word arrives AFTER the cap, we must not
    /// rescind — the speaker spoke too long for it to be a genuine
    /// backchannel. Confirm the interrupt.
    #[test]
    fn backchannel_past_cap_still_confirms() {
        let cfg = BargeInConfig::default();
        // elapsed = 500ms, > max_backchannel_ms (400ms)
        assert_eq!(
            decide(&cfg, 500, "yeah", false),
            BargeInDecision::Confirm
        );
    }

    /// Empty transcript past the rescind delay with user still speaking
    /// should keep waiting — we don't have enough signal yet.
    #[test]
    fn empty_transcript_after_rescind_delay_keeps_waiting() {
        let cfg = BargeInConfig::default();
        assert_eq!(decide(&cfg, 200, "", true), BargeInDecision::Wait);
    }

    /// Punctuation on a backchannel must not defeat detection.
    #[test]
    fn punctuated_backchannel_still_detected() {
        assert!(is_backchannel("Okay."));
        assert!(is_backchannel("Yeah,"));
        assert!(is_backchannel("got it?"));
    }

    /// Empty / whitespace transcript must NOT be classified as a
    /// backchannel — that would let any pause trigger a rescind.
    #[test]
    fn empty_or_whitespace_is_not_a_backchannel() {
        assert!(!is_backchannel(""));
        assert!(!is_backchannel("   "));
        assert!(!is_backchannel("\n\t"));
    }
}
