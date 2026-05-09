//! Phase 13.C — STT/TTS HTTP clients for managed voice backends.
//!
//! Each client is a thin wrapper around `reqwest::Client` that talks
//! to the in-host container managed by Phase 12's BackendSupervisor.
//! Endpoints come from the `config_backends` row for the matching
//! purpose (`VoiceSTT` for Whisper, `VoiceTTS` for Kokoro/Piper) —
//! same hot-reload pattern as `InferenceResolver`.
//!
//! Implementations satisfy the trait surface in
//! `execlaw_voice_pipeline::traits` so the rest of the voice pipeline
//! (VAD, endpointer, bargein) can keep using the trait abstractions
//! and stay testable with `MockStt` / `MockTts` in isolation.
//!
//! Wire format choices:
//!   * **STT request**: PCM16 little-endian mono at 16 kHz, wrapped
//!     in a minimal WAV header so faster-whisper's OpenAI-compat
//!     `/v1/audio/transcriptions` route accepts it. The client
//!     buffers chunks across `push` calls and flushes the
//!     accumulated buffer on `flush` / `peek_partial`.
//!   * **TTS request**: JSON `{"input": "…", "voice": "…"}` against
//!     `/v1/audio/speech`. Response is raw PCM16 LE mono at 24 kHz
//!     (Kokoro's native rate; the SPA-side player resamples).
//!
//! Errors translate to `String` to match the trait signatures —
//! callers log and continue, since a transient STT/TTS failure
//! shouldn't tear down the conversation.
//!
//! Hot reload: clients hold the resolved URL at construction time.
//! `voice_runtime` re-resolves the URL for every new utterance so
//! a Backends save during a conversation picks up on the next turn.

pub mod kokoro;
pub mod whisper;

pub use kokoro::{InterruptHandle, KokoroClient};
pub use whisper::WhisperClient;
