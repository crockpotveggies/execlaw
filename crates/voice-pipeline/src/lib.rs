//! execlaw-voice-pipeline
//!
//! Two-lane Tokio graph (system lane + data lane) composing VAD, STT, LLM,
//! and TTS stages with barge-in and backchannel-rescind semantics. Full
//! implementation lands in Phase 4; Phase 0 ships the frame-vocabulary
//! sketch so downstream crates can typecheck.
//!
//! See MIGRATION_PLAN.md §2.13.2 for the detailed spec.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Frames on the **system lane** pre-empt anything on the data lane.
///
/// Every processor's `tokio::select!` must poll the system lane first, so
/// an `Interruption` reaches every downstream stage within ~milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemFrame {
    UserStartedSpeaking,
    UserStoppedSpeaking,
    Interruption,
    Error(String),
}

/// Frames on the **data lane** are ordered and FIFO.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DataFrame {
    AudioInChunk { start_ms: u64, end_ms: u64 },
    TranscriptPartial(String),
    TranscriptFinal(String),
    LlmTokenDelta(String),
    LlmResponseStart,
    LlmResponseEnd,
    TtsTextChunk(String),
    AudioOutChunk { duration_ms: u32 },
}

/// Defaults (threshold 0.6, min_speech_duration 150ms, min_silence 300–500ms)
/// per §2.13.2.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VadConfig {
    pub threshold: f32,
    pub min_speech_ms: u32,
    pub min_silence_ms: u32,
    pub speech_pad_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: 0.6,
            min_speech_ms: 150,
            min_silence_ms: 400,
            speech_pad_ms: 150,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan() {
        let v = VadConfig::default();
        assert!((v.threshold - 0.6).abs() < 1e-6);
        assert_eq!(v.min_speech_ms, 150);
    }

    #[test]
    fn frames_serialize() {
        let s = serde_json::to_string(&SystemFrame::Interruption).unwrap();
        assert!(s.contains("Interruption"));
    }
}
