//! `VoiceSession` — the orchestrator that glues VAD / STT / LLM /
//! TTS stages onto the two-lane pipeline and writes voice events to
//! the `execlaw-core` event log.
//!
//! Phase 4 scope: drive a full session (user speaks → transcripts →
//! endpoint detection → LLM turn → TTS → barge-in) deterministically
//! against the mock trait implementations in [`crate::traits`]. The
//! real stack (Silero / Whisper / Qwen / Kokoro) plugs in through
//! the same trait surface in Phase 8 without touching this file.

use crate::bargein::{BargeInConfig, BargeInDecision, decide as decide_bargein};
use crate::endpointer::{EndpointerConfig, classify_and_window};
use crate::graph::Pipeline;
use crate::DataFrame;
use crate::traits::{AudioOut, SttClient, SttEvent, TtsClient, Vad, VadDecision};
use execlaw_core::db::Database;
use execlaw_core::events::{EventKind, EventLog, EventRecord};
use execlaw_core::ids::{ConversationId, EventSeq};
use serde::{Deserialize, Serialize};

/// A voice event payload — one per `state_events` row written on
/// the voice path. Mirrors the schema in MIGRATION_PLAN §2.13.6.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceEventPayload {
    /// Milliseconds from the session start — useful for replaying
    /// the temporal shape later (e.g. compute actual EoS → first-audio
    /// latency from a committed log).
    pub t_ms: u64,
    /// Arbitrary JSON per event kind. Stage-specific.
    pub detail: serde_json::Value,
}

/// Current state of the session FSM. Transitions drive what the
/// orchestrator expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Idle — waiting for the user to start speaking.
    Listening,
    /// VAD fired start-speech; STT accumulating partials.
    UserSpeaking,
    /// Endpointer says user stopped; STT flushing; waiting for LLM.
    AwaitingLlm,
    /// LLM emitting tokens; TTS synthesizing + playing.
    AgentSpeaking,
    /// User barge-in detected during AgentSpeaking; held until
    /// [`BargeInConfig`] resolves it into Rescind or Confirm.
    BargeInDecision,
    Ended,
}

/// Static configuration for one session. Per-session runtime state
/// (stage clients, current FSM state, t_ms) lives in [`VoiceSession`].
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub conversation_id: ConversationId,
    pub bargein: BargeInConfig,
    pub endpointer: EndpointerConfig,
    /// When true, every stage transition commits a row to
    /// `state_events`. Tests may disable this to avoid log noise.
    pub write_events: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            conversation_id: ConversationId::from("voice-default"),
            bargein: BargeInConfig::default(),
            endpointer: EndpointerConfig::default(),
            write_events: true,
        }
    }
}

/// One call / one conversation.
///
/// The orchestrator owns:
/// - the two-lane [`Pipeline`] (system lane for interrupts, data
///   lane for stage frames),
/// - the stage-client handles (VAD / STT / TTS), held as trait
///   objects so the mock and real backends swap freely,
/// - the event-log handle (writes voice.* rows on every transition).
///
/// It does *not* own audio I/O — the session is driven by `run_tick`
/// calls from an outer loop that pumps the mic and plays back TTS
/// audio. That split keeps the session deterministic in tests.
pub struct VoiceSession<'db> {
    pub cfg: SessionConfig,
    pub state: SessionState,
    pub pipeline: Pipeline,
    pub vad: Box<dyn Vad>,
    pub stt: Box<dyn SttClient>,
    pub tts: Box<dyn TtsClient>,
    /// Reference to the shared SQLite Database. Each voice event
    /// call builds a fresh `EventLog` against it — cheap (no alloc
    /// beyond the Option<Vec<u8>> for the optional HMAC key clone)
    /// and avoids a self-referential lifetime inside the struct.
    pub db: &'db Database,
    /// HMAC key for event-log signing (§7.8). `None` during tests
    /// and pre-setup.
    pub hmac_key: Option<Vec<u8>>,
    /// Milliseconds since session start.
    pub t_ms: u64,
    /// Latest STT transcript (partial or final) — used by
    /// bargein to decide whether a mid-utterance sound is a
    /// backchannel or a real interrupt.
    pub latest_transcript: String,
    /// When `state == BargeInDecision`: ms the user started
    /// speaking over the agent. Compared to elapsed to drive the
    /// rescind-window decision.
    pub bargein_started_at: Option<u64>,
}

impl<'db> VoiceSession<'db> {
    pub fn new(
        db: &'db Database,
        cfg: SessionConfig,
        vad: Box<dyn Vad>,
        stt: Box<dyn SttClient>,
        tts: Box<dyn TtsClient>,
    ) -> (Self, crate::graph::PipelineEnds) {
        let (pipeline, ends) = Pipeline::new();
        let session = Self {
            cfg,
            state: SessionState::Listening,
            pipeline,
            vad,
            stt,
            tts,
            db,
            hmac_key: None,
            t_ms: 0,
            latest_transcript: String::new(),
            bargein_started_at: None,
        };
        (session, ends)
    }

    /// Attach an HMAC key so the voice event log is tamper-evident
    /// just like the text path (§7.8).
    pub fn with_hmac_key(mut self, key: Vec<u8>) -> Self {
        self.hmac_key = Some(key);
        self
    }

    fn log(&self) -> EventLog<'_> {
        let log = EventLog::new(self.db);
        match &self.hmac_key {
            Some(k) => log.with_hmac_key(k.clone()),
            None => log,
        }
    }

    /// Pump one audio chunk through the session. Returns the VAD
    /// decision so callers can instrument / record.
    pub async fn on_audio_chunk(&mut self, chunk: &crate::traits::AudioChunk) -> VadDecision {
        self.t_ms = chunk.end_ms;
        let decision = self.vad.push(chunk).await;
        match decision {
            VadDecision::SpeechStarted => self.handle_speech_started().await,
            VadDecision::Speaking => {
                self.stt.push(chunk).await;
                if let Some(partial) = self.stt.peek_partial().await {
                    if partial != self.latest_transcript {
                        self.latest_transcript = partial.clone();
                        self.emit(
                            EventKind::SttPartial,
                            serde_json::json!({ "text": partial }),
                        )
                        .await;
                    }
                }
            }
            VadDecision::SpeechEnded => self.handle_speech_ended().await,
            VadDecision::Silence => {}
        }
        decision
    }

    /// Called when the user is mid-speaking WHILE the agent is
    /// talking (state == AgentSpeaking). Holds the decision for up
    /// to `rescind_delay_ms`, then resolves into Rescind (backchannel
    /// — keep talking) or Confirm (real interrupt — halt TTS).
    ///
    /// Returns the final [`BargeInDecision`] once the rescind window
    /// closes; returns `None` while still inside the window.
    pub async fn resolve_bargein(
        &mut self,
        user_still_speaking: bool,
    ) -> Option<BargeInDecision> {
        let started = self.bargein_started_at?;
        let elapsed = self.t_ms.saturating_sub(started) as u32;
        let decision = decide_bargein(
            &self.cfg.bargein,
            elapsed,
            &self.latest_transcript,
            user_still_speaking,
        );
        match decision {
            BargeInDecision::Wait => None,
            BargeInDecision::Rescind => {
                self.bargein_started_at = None;
                self.state = SessionState::AgentSpeaking;
                self.emit(EventKind::InterruptRescinded, serde_json::json!({}))
                    .await;
                Some(BargeInDecision::Rescind)
            }
            BargeInDecision::Confirm => {
                self.bargein_started_at = None;
                self.tts.cancel().await;
                self.pipeline.interrupt();
                self.state = SessionState::UserSpeaking;
                self.emit(EventKind::InterruptConfirmed, serde_json::json!({}))
                    .await;
                Some(BargeInDecision::Confirm)
            }
        }
    }

    /// The full utterance classification (punctuation-aware) after
    /// STT committed a Final. Exposes the computed silence window
    /// so the outer loop can sleep appropriately before deciding
    /// EoS.
    pub fn silence_window_ms(&self) -> u32 {
        classify_and_window(&self.latest_transcript, &self.cfg.endpointer)
    }

    async fn handle_speech_started(&mut self) {
        match self.state {
            SessionState::Listening | SessionState::AwaitingLlm => {
                self.state = SessionState::UserSpeaking;
                self.pipeline.user_started_speaking();
                self.emit(EventKind::VadSpeechStarted, serde_json::json!({}))
                    .await;
            }
            SessionState::AgentSpeaking => {
                // User started talking while the agent is mid-sentence
                // — enter bargein decision.
                self.state = SessionState::BargeInDecision;
                self.bargein_started_at = Some(self.t_ms);
                self.emit(EventKind::InterruptStarted, serde_json::json!({}))
                    .await;
            }
            _ => {}
        }
    }

    async fn handle_speech_ended(&mut self) {
        if self.state == SessionState::UserSpeaking {
            self.state = SessionState::AwaitingLlm;
            self.pipeline.user_stopped_speaking();
            let final_ev = self.stt.flush().await;
            let text = match &final_ev {
                SttEvent::Final { text } => text.clone(),
                SttEvent::Partial { text } => text.clone(),
            };
            self.latest_transcript = text.clone();
            self.emit(EventKind::SttFinal, serde_json::json!({ "text": text }))
                .await;
            self.emit(EventKind::TurnUserEnded, serde_json::json!({}))
                .await;
        }
    }

    /// The outer loop calls this when the LLM has produced its full
    /// reply text (§2.13.6). The session flips to AgentSpeaking,
    /// synthesizes the text chunk-by-chunk, and plays each through
    /// the supplied `AudioOut`. Barge-in is checked between chunks
    /// via `audio_in.next_chunk().await` integrations in the caller.
    pub async fn speak(
        &mut self,
        text: &str,
        audio_out: &mut dyn AudioOut,
    ) -> Result<(), String> {
        self.state = SessionState::AgentSpeaking;
        self.emit(
            EventKind::LlmResponseFinal,
            serde_json::json!({ "text": text }),
        )
        .await;
        let mut first_audio = true;
        for sentence in chunk_at_sentence_boundaries(text) {
            if self.state != SessionState::AgentSpeaking {
                break; // barge-in confirmed
            }
            let tts_out = self.tts.synthesize(&sentence).await?;
            if first_audio {
                self.emit(EventKind::TtsFirstAudio, serde_json::json!({})).await;
                first_audio = false;
            }
            self.emit(
                EventKind::TtsAudioChunk,
                serde_json::json!({ "duration_ms": tts_out.duration_ms }),
            )
            .await;
            audio_out.play_chunk(&tts_out.samples).await?;
            self.pipeline
                .emit(DataFrame::AudioOutChunk {
                    duration_ms: tts_out.duration_ms,
                })
                .await
                .ok();
        }
        if self.state == SessionState::AgentSpeaking {
            self.state = SessionState::Listening;
            self.emit(EventKind::TtsEnded, serde_json::json!({})).await;
        }
        Ok(())
    }

    /// End the session cleanly. Commits a `voice.session_ended` event.
    pub async fn end(&mut self) {
        self.emit(EventKind::VoiceSessionEnded, serde_json::json!({}))
            .await;
        self.state = SessionState::Ended;
    }

    /// Commit a voice event to the log (§2.13.6 wire-up).
    ///
    /// Writes via `EventLog::append` — single-event appends rather
    /// than `commit_turn` because voice events stream continuously
    /// rather than batching as a turn. The pairing invariant
    /// (tool_use/tool_result) isn't relevant for voice events.
    async fn emit(&mut self, kind: EventKind, detail: serde_json::Value) {
        if !self.cfg.write_events {
            return;
        }
        let payload = VoiceEventPayload {
            t_ms: self.t_ms,
            detail,
        };
        let log = self.log();
        let seq = match log.last_seq(&self.cfg.conversation_id) {
            Ok(s) => s.next(),
            Err(e) => {
                tracing::warn!(error = %e, "voice emit: last_seq failed");
                return;
            }
        };
        let ev = match EventRecord::new(
            self.cfg.conversation_id.clone(),
            seq,
            kind,
            &payload,
            Some("voice-session".into()),
        ) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "voice emit: encode failed");
                return;
            }
        };
        if let Err(e) = log.append(&ev) {
            tracing::warn!(error = %e, "voice emit: append failed");
        }
    }

    #[doc(hidden)]
    pub fn for_test_replay_events(&self) -> Result<Vec<EventRecord>, execlaw_core::db::DbError> {
        self.log()
            .replay_since(&self.cfg.conversation_id, EventSeq(0))
    }
}

/// Cut a block of text at sentence boundaries for TTS streaming.
/// Naive splitter — full-stops and new-lines. Good enough for
/// Phase-4 pipeline correctness; real text processing in Phase 8
/// can swap in a proper sentence segmenter.
pub fn chunk_at_sentence_boundaries(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        buf.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n') && !buf.trim().is_empty() {
            out.push(std::mem::take(&mut buf).trim().to_owned());
        }
    }
    let tail = buf.trim();
    if !tail.is_empty() {
        out.push(tail.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{AudioChunk, MockAudioOut, MockStt, MockTts, MockVad};
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn sentence_splitter_handles_simple_punctuation() {
        let out = chunk_at_sentence_boundaries("Hello there. How are you? I am fine!");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "Hello there.");
        assert_eq!(out[1], "How are you?");
        assert_eq!(out[2], "I am fine!");
    }

    #[test]
    fn sentence_splitter_emits_trailing_fragment() {
        let out = chunk_at_sentence_boundaries("ok ");
        assert_eq!(out, vec!["ok".to_string()]);
    }

    #[test]
    fn silence_window_scales_with_tail_punctuation() {
        let db = fresh_db();
        let (mut s, _ends) = VoiceSession::new(
            &db,
            SessionConfig {
                write_events: false,
                ..Default::default()
            },
            Box::new(MockVad::new(vec![])),
            Box::new(MockStt::new(vec![], "hello.".into())),
            Box::new(MockTts::default()),
        );
        s.latest_transcript = "hello.".into();
        let terminal = s.silence_window_ms();
        s.latest_transcript = "hello,".into();
        let midthought = s.silence_window_ms();
        assert!(midthought > terminal);
    }

    /// E2E: user speaks → speech_started + partials + final_ended
    /// events land; state transitions Listening → UserSpeaking →
    /// AwaitingLlm.
    #[tokio::test]
    async fn session_drives_user_speech_to_awaiting_llm() {
        let db = fresh_db();
        let (mut s, _ends) = VoiceSession::new(
            &db,
            SessionConfig {
                conversation_id: ConversationId::from("voice-1"),
                write_events: true,
                ..Default::default()
            },
            Box::new(MockVad::new(vec![
                VadDecision::SpeechStarted,
                VadDecision::Speaking,
                VadDecision::Speaking,
                VadDecision::SpeechEnded,
            ])),
            Box::new(MockStt::new(
                vec!["he".into(), "hello th".into(), "hello there".into()],
                "hello there".into(),
            )),
            Box::new(MockTts::default()),
        );
        let mk_chunk = |t0: u64, t1: u64| AudioChunk {
            start_ms: t0,
            end_ms: t1,
            samples: vec![],
        };
        s.on_audio_chunk(&mk_chunk(0, 20)).await;
        assert_eq!(s.state, SessionState::UserSpeaking);
        s.on_audio_chunk(&mk_chunk(20, 40)).await;
        s.on_audio_chunk(&mk_chunk(40, 60)).await;
        s.on_audio_chunk(&mk_chunk(60, 80)).await;
        assert_eq!(s.state, SessionState::AwaitingLlm);

        // Voice events land in the log: speech_started + 2 partials +
        // stt_final + turn_user_ended.
        let events = s.for_test_replay_events().unwrap();
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::VadSpeechStarted));
        assert!(kinds.contains(&EventKind::SttPartial));
        assert!(kinds.contains(&EventKind::SttFinal));
        assert!(kinds.contains(&EventKind::TurnUserEnded));
    }

    /// E2E: agent speaks → tts.first_audio + tts.audio_chunk (per
    /// sentence) + tts.ended events land; audio samples flow into
    /// MockAudioOut.
    #[tokio::test]
    async fn session_speaks_emits_tts_events_and_audio() {
        let db = fresh_db();
        let (mut s, _ends) = VoiceSession::new(
            &db,
            SessionConfig {
                conversation_id: ConversationId::from("voice-2"),
                ..Default::default()
            },
            Box::new(MockVad::new(vec![])),
            Box::new(MockStt::new(vec![], "".into())),
            Box::new(MockTts::default()),
        );
        let mut audio_out = MockAudioOut::default();
        s.speak("Hello. World!", &mut audio_out).await.unwrap();

        let events = s.for_test_replay_events().unwrap();
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::TtsFirstAudio));
        let n_chunks = kinds
            .iter()
            .filter(|k| **k == EventKind::TtsAudioChunk)
            .count();
        assert_eq!(n_chunks, 2, "two sentences → two TtsAudioChunk events");
        assert!(kinds.contains(&EventKind::TtsEnded));

        let played = audio_out.played.into_inner().unwrap();
        assert_eq!(played.len(), 2);
    }

    /// Barge-in flow: agent is speaking, user starts → InterruptStarted
    /// lands; during the rescind window, `resolve_bargein` returns
    /// None (Wait). If user's transcript becomes a backchannel within
    /// max_backchannel_ms, it Rescinds.
    #[tokio::test]
    async fn bargein_rescinds_on_backchannel_within_window() {
        let db = fresh_db();
        let (mut s, _ends) = VoiceSession::new(
            &db,
            SessionConfig::default(),
            Box::new(MockVad::new(vec![])),
            Box::new(MockStt::new(vec![], "".into())),
            Box::new(MockTts::default()),
        );
        // Fake state: agent was speaking; user barged in at 1000ms.
        s.state = SessionState::BargeInDecision;
        s.bargein_started_at = Some(1000);
        s.latest_transcript = "mm-hmm".into();
        s.t_ms = 1_200; // 200ms after the barge-in start.

        let decision = s.resolve_bargein(false).await;
        assert_eq!(decision, Some(BargeInDecision::Rescind));
        assert_eq!(s.state, SessionState::AgentSpeaking);
    }

    /// Barge-in flow: if user keeps talking past max_backchannel_ms,
    /// bargein confirms and TTS gets canceled.
    #[tokio::test]
    async fn bargein_confirms_when_user_keeps_talking_past_cap() {
        let db = fresh_db();
        let tts = std::sync::Arc::new(MockTts::default());
        // Clone the cancel counter so we can inspect after.
        let canceled = std::sync::Arc::clone(&tts);

        struct TtsWrapper(std::sync::Arc<MockTts>);
        #[async_trait::async_trait]
        impl TtsClient for TtsWrapper {
            async fn synthesize(&mut self, _text: &str) -> Result<crate::traits::TtsAudio, String> {
                unreachable!()
            }
            async fn cancel(&mut self) {
                self.0
                    .canceled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let (mut s, _ends) = VoiceSession::new(
            &db,
            SessionConfig::default(),
            Box::new(MockVad::new(vec![])),
            Box::new(MockStt::new(vec![], "".into())),
            Box::new(TtsWrapper(tts)),
        );
        s.state = SessionState::BargeInDecision;
        s.bargein_started_at = Some(1000);
        s.latest_transcript = "actually wait, hold on".into();
        s.t_ms = 1_600; // 600ms elapsed — past the 400ms backchannel cap.

        let decision = s.resolve_bargein(true).await;
        assert_eq!(decision, Some(BargeInDecision::Confirm));
        assert_eq!(s.state, SessionState::UserSpeaking);
        assert_eq!(
            canceled.canceled.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "TtsClient::cancel must be called on confirm"
        );
    }
}
