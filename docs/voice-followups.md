# Voice mode — known follow-ups

Phase 13.A–13.D shipped end-to-end voice mode with these explicit deferrals.
Each item below is intentionally out of scope for the initial commit; the
hooks are in place so the follow-up is a focused addition rather than a
rewrite.

## 13.E — Server-side WebRTC AEC3 (deferred)

**Status**: deferred to a later phase (13.E-later).

**Why deferred**: WebRTC AEC3 is C++ in upstream WebRTC. Rust bindings are
sparse — `webrtc-audio-processing` lags upstream and Windows builds are
flaky. Honest paths forward are (1) a multi-day FFI yak shave around the
upstream `audio_processing` C++ tree, or (2) a sidecar `service-aec3`
microservice. Neither is justified before voice mode is in active use.

**Current behavior**: browser AEC stays OFF (operator's locked decision in
[`feedback_voice_mode_model`]). The practical implication today is that
**headphones are required** during voice conversations, otherwise the
agent's TTS audio will pick up in the mic and Whisper will transcribe its
own speech. This is acceptable for dogfood; the AEC3 follow-up unlocks
hands-free desktop + speakerphone setups.

**When to revisit**: when phone-bridge (Bluetooth → execlaw) sources land,
because phone audio comes back in the mic by definition and we can't
require headphones on a phone bridge.

## SPA mic capture: Opus → PCM16 (deferred)

**Status**: VoiceCaptureButton emits Opus chunks via MediaRecorder
(`audio/webm;codecs=opus` on Chromium/Firefox, `audio/mp4` on Safari).
Server-side `voice_runtime::ingest_chunks` only accepts `pcm16le` /
`pcm16`; non-PCM frames are logged at WARN and dropped.

**Implication**: in this commit, voice mode round-trips successfully for
TTS playback (server emits PCM, SPA decodes via `VoicePlayback`) but the
Whisper round-trip is wired but inert until the codec gap closes.

**Two paths forward**:

1. **Client-side PCM capture** — replace the MediaRecorder path with an
   `AudioWorkletNode` that emits raw PCM16 at 16 kHz. Smallest change to
   the protocol; loses Opus's bandwidth advantage (acceptable on a LAN /
   self-hosted setup).
2. **Server-side Opus decoder** — add the `opus` crate to
   `crates/server`. Decodes inbound Opus → PCM16 before handing to
   `voice_runtime`. Keeps the SPA path unchanged. Adds a native dep that
   may complicate the Windows-host dev story but builds cleanly on Linux.

Recommendation for the follow-up commit: option 1 (client-side AudioWorklet)
since it also enables future mobile native + phone-bridge sources to send
PCM uniformly without negotiating per-source codecs.

## Continuous VAD-driven endpointing (deferred)

**Status**: v1 voice mode is push-to-talk. The SPA's mic toggle generates
the `voice_stop` control message; the server's voice_runtime flushes
Whisper on receipt.

**Follow-up**: server-side WebRTC-VAD (or Silero ONNX in the
`voice-pipeline` crate, which already has the `Vad` trait) auto-endpoints
on silence so the operator can speak hands-free. Wire format change is
zero — same `voice_stop` UiEvent, just emitted by the server instead of
the SPA.

## Real chat-path agent reply on voice_stop (deferred)

**Status**: the `voice_stop` control handler currently echoes the
transcript back as `you said: <text>` instead of routing through the
chat / runner / LLM path. Verifies the round-trip without coupling
`voice_runtime` to `chats::dispatch_turn`.

**Follow-up**: replace the echo callback with a thin adapter that opens a
conversation against the controller's thread, posts the transcript as a
`ChatMessageInbound`, and consumes the resulting `ChatTokenDelta` /
`ChatMessageOutbound` stream as the TTS source. The voice pipeline already
streams TTS chunks per call; the adapter just feeds it the runner's
output sentence-by-sentence.

## Streaming TTS feedback to the runner (deferred)

**Status**: barge-in fires `KokoroClient::cancel()` and SPA-side
`VoicePlayback.flush()`, but the runner doesn't know how much of its
reply was actually heard before the interrupt.

**Follow-up**: track `played_through_sentence` in the runtime's
session state (incremented when Kokoro's audio chunk for sentence N has
finished synthesizing). On `voice_interrupt`, log a structured
`VoiceInterrupted { played_through_sentence: u32 }` event so future
analytics can distinguish "user barged in immediately" from "user heard
most of it." Conversation history then truncates the agent's "outbound"
message to the actually-heard prefix, matching what humans would do.
