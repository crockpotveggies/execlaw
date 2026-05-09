//! Trait interfaces between the voice pipeline and its stage backends.
//!
//! Every external runtime (Silero VAD ONNX, faster-whisper,
//! OpenVINO Kokoro, WebRTC AEC3, the mic/speaker device layer) lives
//! behind one of these traits so the pipeline itself stays pure Rust
//! and deterministically testable.
//!
//! Phase 4 scope: define the contracts + mock implementations so the
//! pipeline's composition, bargein, endpointing, and event-log wiring
//! can be proven end-to-end without the real model containers. The
//! real backends land with Phase 8 (`service-whisper`,
//! `service-kokoro`, etc.) as sidecar containers the plugin-host
//! manages.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Audio I/O
// ---------------------------------------------------------------------------

/// One PCM chunk flowing into the pipeline from the microphone.
/// Sample rate is 16 kHz mono unless documented otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioChunk {
    pub start_ms: u64,
    pub end_ms: u64,
    /// 16-bit signed PCM, little-endian, mono.
    pub samples: Vec<i16>,
}

impl AudioChunk {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// Read side of a microphone transport. Implementations stream
/// `AudioChunk`s at ~20ms cadence (typical for VAD + STT).
#[async_trait]
pub trait AudioIn: Send + Sync {
    /// Block until the next chunk is available. Return `None` on EOF
    /// (mic disconnected, test fixture exhausted).
    async fn next_chunk(&mut self) -> Option<AudioChunk>;
}

/// Write side of a speaker transport. TTS audio lands here.
#[async_trait]
pub trait AudioOut: Send + Sync {
    async fn play_chunk(&mut self, samples: &[i16]) -> Result<(), String>;
    /// Halt playback immediately. Called on barge-in.
    async fn stop(&mut self);
}

// ---------------------------------------------------------------------------
// Voice activity detection
// ---------------------------------------------------------------------------

/// One VAD decision per audio chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VadDecision {
    /// Speech just started (the first voiced chunk after silence).
    SpeechStarted,
    /// Still inside a speech region.
    Speaking,
    /// Speech just ended.
    SpeechEnded,
    /// Silence.
    Silence,
}

/// Voice-activity detector. Stateful: each call feeds a new chunk
/// and returns the transition (if any). Implementations decide based
/// on energy + ML model + hysteresis.
#[async_trait]
pub trait Vad: Send + Sync {
    async fn push(&mut self, chunk: &AudioChunk) -> VadDecision;
}

// ---------------------------------------------------------------------------
// STT
// ---------------------------------------------------------------------------

/// A partial or final transcript emitted by the STT backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttEvent {
    /// Incremental transcript — may change between emissions.
    Partial { text: String },
    /// Final transcript for this utterance; STT commits to it.
    Final { text: String },
}

/// Streaming speech-to-text. The implementation receives a stream
/// of audio chunks via `push` and emits `Partial` / `Final`
/// transcripts via the returned channel. `flush` forces a final
/// decision (e.g. when the endpointer says user stopped).
#[async_trait]
pub trait SttClient: Send + Sync {
    async fn push(&mut self, chunk: &AudioChunk);
    /// Finalize the current utterance and return the Final event.
    async fn flush(&mut self) -> SttEvent;
    /// Get the latest partial (if any) without flushing.
    async fn peek_partial(&self) -> Option<String>;
    /// Reset state between utterances / on barge-in.
    async fn reset(&mut self);
}

// ---------------------------------------------------------------------------
// TTS
// ---------------------------------------------------------------------------

/// One TTS output — the synthesized PCM for a chunk of text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TtsAudio {
    pub text: String,
    pub samples: Vec<i16>,
    pub duration_ms: u32,
}

/// Text-to-speech. `synthesize` returns audio for one text chunk;
/// typical usage is to chunk the LLM's output at sentence boundaries
/// and synthesize in parallel so first-audio latency stays low.
#[async_trait]
pub trait TtsClient: Send + Sync {
    async fn synthesize(&mut self, text: &str) -> Result<TtsAudio, String>;
    /// Halt any in-flight synthesis; called on barge-in.
    async fn cancel(&mut self);
}

// ---------------------------------------------------------------------------
// Mocks (only compiled in dev/test)
// ---------------------------------------------------------------------------

/// Fixture `AudioIn` that yields a pre-recorded sequence of chunks.
pub struct MockAudioIn {
    chunks: std::collections::VecDeque<AudioChunk>,
}

impl MockAudioIn {
    pub fn new(chunks: Vec<AudioChunk>) -> Self {
        Self {
            chunks: chunks.into(),
        }
    }
}

#[async_trait]
impl AudioIn for MockAudioIn {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        self.chunks.pop_front()
    }
}

/// In-memory `AudioOut` that collects every chunk for test assertions.
#[derive(Default)]
pub struct MockAudioOut {
    pub played: std::sync::Mutex<Vec<Vec<i16>>>,
    pub stops: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl AudioOut for MockAudioOut {
    async fn play_chunk(&mut self, samples: &[i16]) -> Result<(), String> {
        self.played.lock().unwrap().push(samples.to_vec());
        Ok(())
    }
    async fn stop(&mut self) {
        self.stops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Mock VAD driven by an explicit decision script. Each call to
/// `push` consumes one decision from the script. Useful for driving
/// the pipeline deterministically in tests.
pub struct MockVad {
    script: std::collections::VecDeque<VadDecision>,
}

impl MockVad {
    pub fn new(script: Vec<VadDecision>) -> Self {
        Self {
            script: script.into(),
        }
    }
}

#[async_trait]
impl Vad for MockVad {
    async fn push(&mut self, _chunk: &AudioChunk) -> VadDecision {
        self.script.pop_front().unwrap_or(VadDecision::Silence)
    }
}

/// Mock STT that returns a pre-scripted transcript on `flush` and
/// emits partial deltas as chunks arrive.
pub struct MockStt {
    pending_partial: Option<String>,
    final_text: String,
    partials: std::collections::VecDeque<String>,
}

impl MockStt {
    /// `partials` are emitted one per `push`; `final_text` returns
    /// from `flush`.
    pub fn new(partials: Vec<String>, final_text: String) -> Self {
        Self {
            pending_partial: None,
            final_text,
            partials: partials.into(),
        }
    }
}

#[async_trait]
impl SttClient for MockStt {
    async fn push(&mut self, _chunk: &AudioChunk) {
        if let Some(next) = self.partials.pop_front() {
            self.pending_partial = Some(next);
        }
    }
    async fn flush(&mut self) -> SttEvent {
        let text = std::mem::take(&mut self.final_text);
        self.pending_partial = None;
        SttEvent::Final { text }
    }
    async fn peek_partial(&self) -> Option<String> {
        self.pending_partial.clone()
    }
    async fn reset(&mut self) {
        self.pending_partial = None;
        self.partials.clear();
    }
}

// ---------------------------------------------------------------------------
// Deep-runner escalation (§2.9 case 3)
// ---------------------------------------------------------------------------

/// A "deep runner" the voice path can escalate hard prompts to
/// while the primary turn emits filler audio. The voice runner is
/// optimized for low first-audio latency; some user requests
/// genuinely need more reasoning depth than that budget allows.
///
/// The flow (verified against `MockDeepRunner` below):
///
/// 1. Voice runner detects a "needs deeper reasoning" hint in the
///    transcript or LLM response (§2.9: explicit `i_need_to_think`
///    tool call, or a heuristic on the transcript content).
/// 2. Voice runner emits a short filler ("one sec, let me check").
/// 3. Filler is synthesized + played WHILE the deep runner grinds.
/// 4. When the deep runner finishes, its response gets synthesized
///    into the next chunks.
///
/// Phase 4 ships the trait + a Mock; the production deep-runner is
/// a chat-path turn against the Reasoning deployment (§5.4) on a
/// nvidia GPU, plumbed through Phase 8 when the hybrid GPU runtime
/// is configured.
#[async_trait]
pub trait DeepRunner: Send + Sync {
    /// Run the prompt to completion. Latency is allowed to be many
    /// seconds — the caller emits filler in parallel.
    async fn answer(&mut self, prompt: &str) -> Result<String, String>;
}

/// In-memory mock that returns a pre-scripted answer after an
/// optional simulated delay. Useful for testing the
/// filler-while-grinds choreography without a live model.
pub struct MockDeepRunner {
    pub answer: String,
    pub delay: std::time::Duration,
}

impl MockDeepRunner {
    pub fn new(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
            delay: std::time::Duration::from_millis(0),
        }
    }
    pub fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait]
impl DeepRunner for MockDeepRunner {
    async fn answer(&mut self, _prompt: &str) -> Result<String, String> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(self.answer.clone())
    }
}

/// Mock TTS that returns constant-length synthetic audio.
pub struct MockTts {
    pub canceled: std::sync::atomic::AtomicUsize,
}

impl Default for MockTts {
    fn default() -> Self {
        Self {
            canceled: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl TtsClient for MockTts {
    async fn synthesize(&mut self, text: &str) -> Result<TtsAudio, String> {
        // 200ms of silence per character — deterministic shape for tests.
        let duration_ms = 10 * text.chars().count() as u32;
        let n_samples = (duration_ms as usize * 16_000) / 1000;
        Ok(TtsAudio {
            text: text.to_owned(),
            samples: vec![0i16; n_samples],
            duration_ms,
        })
    }
    async fn cancel(&mut self) {
        self.canceled
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_audio_in_drains_to_none() {
        let chunk = AudioChunk {
            start_ms: 0,
            end_ms: 20,
            samples: vec![0; 320],
        };
        let mut m = MockAudioIn::new(vec![chunk.clone()]);
        assert_eq!(m.next_chunk().await, Some(chunk));
        assert_eq!(m.next_chunk().await, None);
    }

    #[tokio::test]
    async fn mock_vad_follows_script_then_silences() {
        let mut v = MockVad::new(vec![
            VadDecision::SpeechStarted,
            VadDecision::Speaking,
            VadDecision::SpeechEnded,
        ]);
        let c = AudioChunk {
            start_ms: 0,
            end_ms: 20,
            samples: vec![],
        };
        assert_eq!(v.push(&c).await, VadDecision::SpeechStarted);
        assert_eq!(v.push(&c).await, VadDecision::Speaking);
        assert_eq!(v.push(&c).await, VadDecision::SpeechEnded);
        // Script exhausted → falls back to Silence.
        assert_eq!(v.push(&c).await, VadDecision::Silence);
    }

    #[tokio::test]
    async fn mock_stt_roundtrip() {
        let mut s = MockStt::new(vec!["he".into(), "hello".into()], "hello there".into());
        let c = AudioChunk {
            start_ms: 0,
            end_ms: 20,
            samples: vec![],
        };
        s.push(&c).await;
        assert_eq!(s.peek_partial().await.as_deref(), Some("he"));
        s.push(&c).await;
        assert_eq!(s.peek_partial().await.as_deref(), Some("hello"));
        match s.flush().await {
            SttEvent::Final { text } => assert_eq!(text, "hello there"),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_tts_synthesize_deterministic_length() {
        let mut t = MockTts::default();
        let a = t.synthesize("hi").await.unwrap();
        assert_eq!(a.duration_ms, 20);
        assert_eq!(a.samples.len(), 320);
        assert_eq!(a.text, "hi");
    }

    #[tokio::test]
    async fn mock_tts_cancel_increments_counter() {
        let mut t = MockTts::default();
        t.cancel().await;
        t.cancel().await;
        assert_eq!(t.canceled.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
