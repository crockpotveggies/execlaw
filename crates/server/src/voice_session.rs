//! Phase 13.B — server-side voice-session registry + jitter buffer.
//!
//! Owns the cross-frame state for inbound voice mode: which
//! sessions are active, the per-session reorder buffer, the codec
//! metadata, and the lifecycle events that drive the SPA's mic UI.
//!
//! `VoiceSessionRegistry` is the single piece events.rs talks to:
//!
//! ```text
//! events.rs::stream_handler
//!     -> binary frame arrives
//!     -> voice_frame::parse_frame  (Phase 13.A — header + payload)
//!     -> registry.observe_frame    (this module — session + jitter)
//!     -> emits ordered chunks
//!     -> WhisperClient adapter     (Phase 13.C — real STT)
//!     -> VoiceTranscript events    (Phase 13.B — wire shape ready)
//! ```
//!
//! Phase 13.B is the registry + buffer + lifecycle events; the
//! "real STT" arm is still a stub. Each ordered chunk emits a
//! `VoiceTranscript { is_final: false }` placeholder so the SPA
//! can confirm the round-trip.
//!
//! The jitter buffer is intentionally small (8 frames). Voice
//! mode prioritizes latency over completeness — at 250ms timeslices
//! that's a 2-second window, well past any reasonable network
//! reordering on a LAN or cellular link.

use crate::events::{EventBus, UiEvent};
use crate::voice_frame::VoiceFrameHeader;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Maximum number of frames buffered per session before the oldest
/// is forced out and dispatched (with a "frame_dropped" log) so
/// downstream pipelines don't stall.
pub const MAX_BUFFER_FRAMES: usize = 8;

/// How long a session can sit idle before the reaper sweeps it.
/// Browsers send chunks every ~250ms, so 30s of silence is well
/// past "operator clicked away."
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// One ordered audio chunk after the jitter buffer has done its
/// work. Carries enough metadata for the WhisperClient adapter
/// (Phase 13.C) to decode without re-reading the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedAudioChunk {
    pub session: String,
    pub seq: u32,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct Session {
    /// Once-per-session metadata. Subsequent frames for the same
    /// session whose codec / sample_rate disagree are logged and
    /// the first-set value wins.
    codec: String,
    sample_rate: u32,
    channels: u32,
    /// Next seq we expect to dispatch in order.
    next_expected_seq: u32,
    /// Out-of-order frames waiting for their turn. Keyed by seq;
    /// dispatched as `next_expected_seq` catches up.
    pending: BTreeMap<u32, Vec<u8>>,
    /// Wall-clock of the last observed frame. Drives the reaper.
    last_seen_at: Instant,
}

impl Session {
    fn new(header: &VoiceFrameHeader, now: Instant) -> Self {
        Self {
            codec: header.codec.clone(),
            sample_rate: header.sample_rate,
            channels: header.channels.max(1),
            next_expected_seq: 0,
            pending: BTreeMap::new(),
            last_seen_at: now,
        }
    }
}

/// Outcomes of observing a single inbound frame. Returned to the
/// caller (events.rs) so it can publish the right events without
/// the registry reaching back into the bus.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// True if this frame opened a brand-new session. Caller
    /// publishes `UiEvent::VoiceSessionStarted`.
    pub session_opened: bool,
    /// Ordered chunks the registry was able to release after
    /// inserting this frame (the new frame plus any pending
    /// backlog whose turn finally came up).
    pub released: Vec<OrderedAudioChunk>,
    /// Frames the buffer evicted to keep the queue under
    /// `MAX_BUFFER_FRAMES`. Caller logs but doesn't fail the turn.
    pub evicted_seqs: Vec<u32>,
}

/// In-memory registry. Cheap to clone — one `Arc<Mutex<...>>`
/// inside, shared across the WS handler + the jitter-buffer reaper.
#[derive(Clone)]
pub struct VoiceSessionRegistry {
    inner: Arc<Mutex<Inner>>,
    events: EventBus,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
}

impl VoiceSessionRegistry {
    pub fn new(events: EventBus) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            events,
        }
    }

    /// Number of live sessions. Lets observability surface
    /// "currently N voice sessions" without grabbing the lock
    /// twice.
    pub async fn live_count(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// Process one parsed frame. Returns chunks ready for the STT
    /// pipeline + lifecycle hints the caller publishes.
    pub async fn observe_frame(
        &self,
        header: &VoiceFrameHeader,
        payload: &[u8],
    ) -> FrameOutcome {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let mut outcome = FrameOutcome::default();

        let session = inner
            .sessions
            .entry(header.session.clone())
            .or_insert_with(|| {
                outcome.session_opened = true;
                Session::new(header, now)
            });
        session.last_seen_at = now;

        // Sanity-check that the codec / sample_rate didn't change
        // mid-session. If they did, the source has a bug; we keep
        // the original metadata so the STT decoder doesn't
        // mid-stream-swap (which corrupts decoded audio).
        if session.codec != header.codec {
            warn!(
                session = %header.session,
                "voice frame codec changed mid-session ({} -> {}); ignoring update",
                session.codec, header.codec
            );
        }

        // Stale frame from a far-back seq we already dispatched —
        // drop silently, no eviction needed.
        if header.seq < session.next_expected_seq {
            debug!(
                session = %header.session,
                seq = header.seq,
                expected = session.next_expected_seq,
                "stale voice frame dropped (seq < expected)"
            );
            return outcome;
        }

        session.pending.insert(header.seq, payload.to_vec());

        // Drain everything we now have in order.
        while let Some(payload) = session.pending.remove(&session.next_expected_seq) {
            outcome.released.push(OrderedAudioChunk {
                session: header.session.clone(),
                seq: session.next_expected_seq,
                codec: session.codec.clone(),
                sample_rate: session.sample_rate,
                channels: session.channels,
                payload,
            });
            session.next_expected_seq = session.next_expected_seq.wrapping_add(1);
        }

        // Cap pending; force-release the oldest if a sender's been
        // dropping frames for too long.
        while session.pending.len() > MAX_BUFFER_FRAMES {
            // BTreeMap pops in seq order; the smallest key is the
            // earliest gap that's blocking dispatch. Bump
            // next_expected_seq past it so the next iteration
            // releases what's behind it.
            if let Some((&oldest_seq, _payload)) = session.pending.iter().next() {
                outcome.evicted_seqs.push(oldest_seq);
                session.next_expected_seq = oldest_seq.wrapping_add(1);
                // Re-drain — the bump may have unblocked more.
                while let Some(payload) =
                    session.pending.remove(&session.next_expected_seq)
                {
                    outcome.released.push(OrderedAudioChunk {
                        session: header.session.clone(),
                        seq: session.next_expected_seq,
                        codec: session.codec.clone(),
                        sample_rate: session.sample_rate,
                        channels: session.channels,
                        payload,
                    });
                    session.next_expected_seq =
                        session.next_expected_seq.wrapping_add(1);
                }
                // If we unblocked the sentinel that triggered the
                // overflow, we're done.
                if session.pending.len() <= MAX_BUFFER_FRAMES {
                    break;
                }
            }
        }

        outcome
    }

    /// Publish lifecycle events + Phase-13.B-stub VoiceTranscript
    /// events for each released chunk. Phase 13.C swaps the stub
    /// for real Whisper output.
    pub fn publish_outcome(&self, header: &VoiceFrameHeader, outcome: &FrameOutcome) {
        if outcome.session_opened {
            self.events.publish(UiEvent::VoiceSessionStarted {
                session: header.session.clone(),
                codec: header.codec.clone(),
                sample_rate: header.sample_rate,
            });
        }
        for chunk in &outcome.released {
            // Phase 13.B stub — emit a placeholder transcript so
            // the SPA's WS handler can be wired before Whisper
            // lands. Phase 13.C replaces this with the real
            // adapter.
            self.events.publish(UiEvent::VoiceTranscript {
                session: chunk.session.clone(),
                seq: chunk.seq,
                text: format!("[stub seq={} bytes={}]", chunk.seq, chunk.payload.len()),
                is_final: false,
            });
        }
        for evicted_seq in &outcome.evicted_seqs {
            warn!(
                session = %header.session,
                evicted_seq = evicted_seq,
                "voice jitter buffer evicted oldest pending frame"
            );
        }
    }

    /// Drop a session and publish `VoiceSessionEnded`. Called by
    /// the operator-stop path (SPA toggles mic off → the WS
    /// handler currently has no signal for this; that hook lands
    /// when 13.B's text-message protocol gets a `voice_stop`
    /// op-code) AND by the reaper for stale sessions.
    pub async fn end_session(&self, session_id: &str, reason: &str) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.sessions.remove(session_id).is_some() {
            self.events.publish(UiEvent::VoiceSessionEnded {
                session: session_id.to_owned(),
                reason: reason.to_owned(),
            });
            true
        } else {
            false
        }
    }

    /// Sweep sessions whose last frame is older than
    /// `SESSION_IDLE_TIMEOUT`. Returns the number reaped.
    pub async fn reap_stale(&self) -> usize {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let stale: Vec<String> = inner
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                if now.duration_since(s.last_seen_at) > SESSION_IDLE_TIMEOUT {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        for id in &stale {
            inner.sessions.remove(id);
            self.events.publish(UiEvent::VoiceSessionEnded {
                session: id.clone(),
                reason: "stale_reap".to_owned(),
            });
        }
        stale.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;

    fn header(session: &str, seq: u32) -> VoiceFrameHeader {
        VoiceFrameHeader {
            session: session.into(),
            seq,
            codec: "opus".into(),
            sample_rate: 48000,
            channels: 1,
            ts_ms: None,
        }
    }

    #[tokio::test]
    async fn first_frame_opens_a_new_session() {
        let bus = EventBus::new();
        let reg = VoiceSessionRegistry::new(bus);
        let outcome = reg.observe_frame(&header("s1", 0), b"chunk0").await;
        assert!(outcome.session_opened);
        assert_eq!(outcome.released.len(), 1);
        assert_eq!(outcome.released[0].seq, 0);
        assert_eq!(reg.live_count().await, 1);
    }

    #[tokio::test]
    async fn in_order_frames_release_immediately() {
        let bus = EventBus::new();
        let reg = VoiceSessionRegistry::new(bus);
        for s in 0..5 {
            let outcome = reg.observe_frame(&header("s1", s), b"x").await;
            assert_eq!(outcome.released.len(), 1);
            assert_eq!(outcome.released[0].seq, s);
        }
    }

    #[tokio::test]
    async fn out_of_order_frames_wait_then_release_in_sequence() {
        let bus = EventBus::new();
        let reg = VoiceSessionRegistry::new(bus);
        // 0, 2, 1, 3 — should release as 0, then 1+2 together,
        // then 3.
        let o0 = reg.observe_frame(&header("s1", 0), b"a").await;
        assert_eq!(o0.released.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![0]);
        let o2 = reg.observe_frame(&header("s1", 2), b"c").await;
        assert!(o2.released.is_empty(), "seq 2 must wait for seq 1");
        let o1 = reg.observe_frame(&header("s1", 1), b"b").await;
        assert_eq!(
            o1.released.iter().map(|c| c.seq).collect::<Vec<_>>(),
            vec![1, 2],
            "filling the gap releases the backlog in seq order",
        );
        let o3 = reg.observe_frame(&header("s1", 3), b"d").await;
        assert_eq!(o3.released.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![3]);
    }

    #[tokio::test]
    async fn stale_seq_is_dropped_silently() {
        let bus = EventBus::new();
        let reg = VoiceSessionRegistry::new(bus);
        // Establish next_expected_seq = 1 by releasing seq 0.
        reg.observe_frame(&header("s1", 0), b"a").await;
        // A late re-send of seq 0 — already dispatched.
        let stale = reg.observe_frame(&header("s1", 0), b"a-dup").await;
        assert!(stale.released.is_empty());
        assert!(stale.evicted_seqs.is_empty());
    }

    #[tokio::test]
    async fn buffer_eviction_kicks_in_at_max_frames() {
        let bus = EventBus::new();
        let reg = VoiceSessionRegistry::new(bus);
        // Hold seq 0 by sending 1..=MAX_BUFFER_FRAMES + 1 first.
        // Each is buffered (next_expected_seq stays 0).
        for s in 1..=(MAX_BUFFER_FRAMES as u32 + 1) {
            let _ = reg.observe_frame(&header("s1", s), b"x").await;
        }
        // The (MAX+1)th queued frame must trigger eviction of seq
        // 0 (the missing frame at the front of the line) so the
        // pipeline doesn't stall forever.
        let inner = reg.inner.lock().await;
        let session = inner.sessions.get("s1").unwrap();
        assert!(
            session.pending.len() <= MAX_BUFFER_FRAMES,
            "pending must be capped at MAX_BUFFER_FRAMES; got {}",
            session.pending.len()
        );
    }

    #[tokio::test]
    async fn end_session_publishes_voice_session_ended() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let reg = VoiceSessionRegistry::new(bus);
        reg.observe_frame(&header("s1", 0), b"a").await;
        let removed = reg.end_session("s1", "operator_stopped").await;
        assert!(removed);

        // Drain events; expect VoiceSessionEnded for s1 with
        // reason=operator_stopped.
        let mut saw_end = false;
        for _ in 0..16 {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(UiEvent::VoiceSessionEnded { session, reason })) => {
                    if session == "s1" && reason == "operator_stopped" {
                        saw_end = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_end, "VoiceSessionEnded must be published");
        assert_eq!(reg.live_count().await, 0);
    }

    #[tokio::test]
    async fn end_session_unknown_id_returns_false() {
        let reg = VoiceSessionRegistry::new(EventBus::new());
        assert!(!reg.end_session("never-existed", "operator_stopped").await);
    }

    #[tokio::test]
    async fn reap_stale_drops_idle_sessions_only() {
        let bus = EventBus::new();
        let reg = VoiceSessionRegistry::new(bus);
        reg.observe_frame(&header("fresh", 0), b"a").await;
        reg.observe_frame(&header("idle", 0), b"a").await;
        // Forge `idle`'s last_seen_at into the deep past.
        {
            let mut inner = reg.inner.lock().await;
            if let Some(s) = inner.sessions.get_mut("idle") {
                s.last_seen_at = Instant::now() - SESSION_IDLE_TIMEOUT - Duration::from_secs(5);
            }
        }
        let reaped = reg.reap_stale().await;
        assert_eq!(reaped, 1);
        assert_eq!(reg.live_count().await, 1);
        let inner = reg.inner.lock().await;
        assert!(inner.sessions.contains_key("fresh"));
    }

    #[tokio::test]
    async fn publish_outcome_emits_session_started_then_transcripts() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let reg = VoiceSessionRegistry::new(bus);
        let h = header("s1", 0);
        let outcome = reg.observe_frame(&h, b"chunk").await;
        reg.publish_outcome(&h, &outcome);

        let mut saw_start = false;
        let mut saw_transcript = false;
        for _ in 0..16 {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(UiEvent::VoiceSessionStarted { session, .. })) if session == "s1" => {
                    saw_start = true;
                }
                Ok(Ok(UiEvent::VoiceTranscript { session, seq, .. }))
                    if session == "s1" && seq == 0 =>
                {
                    saw_transcript = true;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
            if saw_start && saw_transcript {
                break;
            }
        }
        assert!(saw_start);
        assert!(saw_transcript);
    }
}
