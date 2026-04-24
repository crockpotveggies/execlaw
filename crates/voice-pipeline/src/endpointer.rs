//! Punctuation-aware dynamic-silence endpointer (§2.13.3 Phase 1).
//!
//! Instead of a model-based turn-detector (like LiveKit's Qwen2.5-0.5B
//! fine-tune) — which the user chose to skip in favor of a lighter
//! approach — we read the tail punctuation of the latest STT partial
//! and shorten or lengthen the silence window accordingly:
//!
//! - Terminal punctuation (`.`, `?`, `!`) → likely end-of-turn, short
//!   silence window (~200-300ms) before declaring end-of-utterance.
//! - Continuation (`,`, `:`, `;`) → mid-thought; extend silence window
//!   to ~800-1000ms so the user can finish their sentence.
//! - No punctuation at all → default silence window (~500ms).
//!
//! Zero model calls, zero extra inference cost.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointHint {
    /// Most recent transcript ends with a terminal punctuation mark.
    Terminal,
    /// Most recent transcript ends mid-thought.
    MidThought,
    /// Can't tell from punctuation alone.
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EndpointerConfig {
    /// Silence window to wait after terminal punctuation.
    pub terminal_ms: u32,
    /// Silence window to wait after mid-thought.
    pub mid_thought_ms: u32,
    /// Silence window when punctuation gives no hint.
    pub default_ms: u32,
}

impl Default for EndpointerConfig {
    fn default() -> Self {
        Self {
            terminal_ms: 250,
            mid_thought_ms: 900,
            default_ms: 500,
        }
    }
}

/// Classify the tail of a transcript into an endpoint hint.
pub fn classify_tail(transcript: &str) -> EndpointHint {
    let trimmed = transcript.trim_end();
    let Some(last) = trimmed.chars().last() else {
        return EndpointHint::Unknown;
    };
    match last {
        // ASCII + CJK terminal punctuation
        '.' | '?' | '!' | '。' => EndpointHint::Terminal,
        ',' | ':' | ';' => EndpointHint::MidThought,
        _ => EndpointHint::Unknown,
    }
}

/// How long to wait in silence before declaring end-of-utterance.
pub fn silence_window_ms(cfg: &EndpointerConfig, hint: EndpointHint) -> u32 {
    match hint {
        EndpointHint::Terminal => cfg.terminal_ms,
        EndpointHint::MidThought => cfg.mid_thought_ms,
        EndpointHint::Unknown => cfg.default_ms,
    }
}

/// Convenience one-shot.
pub fn classify_and_window(transcript: &str, cfg: &EndpointerConfig) -> u32 {
    silence_window_ms(cfg, classify_tail(transcript))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_punctuation_shortens_silence() {
        let cfg = EndpointerConfig::default();
        assert_eq!(classify_tail("Hello there."), EndpointHint::Terminal);
        assert_eq!(classify_tail("Who is this?"), EndpointHint::Terminal);
        assert_eq!(silence_window_ms(&cfg, EndpointHint::Terminal), 250);
    }

    #[test]
    fn comma_lengthens_silence() {
        let cfg = EndpointerConfig::default();
        assert_eq!(classify_tail("well,"), EndpointHint::MidThought);
        assert!(
            silence_window_ms(&cfg, EndpointHint::MidThought)
                > silence_window_ms(&cfg, EndpointHint::Unknown)
        );
    }

    #[test]
    fn no_punctuation_returns_unknown_with_default_window() {
        let cfg = EndpointerConfig::default();
        assert_eq!(classify_tail("hello"), EndpointHint::Unknown);
        assert_eq!(silence_window_ms(&cfg, EndpointHint::Unknown), 500);
    }

    #[test]
    fn ignores_trailing_whitespace() {
        assert_eq!(classify_tail("done.  \n"), EndpointHint::Terminal);
    }

    #[test]
    fn empty_transcript_is_unknown() {
        assert_eq!(classify_tail(""), EndpointHint::Unknown);
        assert_eq!(classify_tail("   "), EndpointHint::Unknown);
    }

    #[test]
    fn cjk_terminal_punctuation_handled() {
        assert_eq!(classify_tail("こんにちは。"), EndpointHint::Terminal);
    }
}
