//! Phase 13.D — background reaper for stale voice sessions.
//!
//! Voice sessions are observable via the SPA's mic UX; if the
//! operator closes the tab mid-session, the registry has no signal
//! and the session sits forever. The reaper runs a periodic
//! `reap_stale` against `VoiceSessionRegistry` (5s cadence) and
//! also drops the matching `voice_runtime` per-session state so
//! both maps stay in sync.
//!
//! The task carries a stop signal so SIGTERM drains it cleanly
//! along with the rest of the Phase-7 background workers
//! (`log_retention`, `ephemeral_sweeper`, `refresh_tokens`).

use crate::voice_runtime::VoiceRuntime;
use crate::voice_session::VoiceSessionRegistry;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::debug;

/// How often the reaper sweeps stale sessions. Smaller than
/// `SESSION_IDLE_TIMEOUT` (30s) so a stale session is observed
/// within ~5s of crossing the threshold.
pub const DEFAULT_REAP_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn a tokio task that periodically reaps stale sessions until
/// `stop` fires. Returns the JoinHandle so the caller can `.await`
/// it on shutdown if it wants drain semantics; production fires a
/// hard SIGTERM so we don't bother in `cli/main.rs`.
pub fn spawn(
    registry: VoiceSessionRegistry,
    runtime: VoiceRuntime,
    stop: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    spawn_with_interval(registry, runtime, stop, DEFAULT_REAP_INTERVAL)
}

/// Same as [`spawn`] but with a custom interval — used by tests to
/// drive the reap loop without waiting 5s. Production callers use
/// [`spawn`].
pub fn spawn_with_interval(
    registry: VoiceSessionRegistry,
    runtime: VoiceRuntime,
    stop: Arc<Notify>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { run(registry, runtime, stop, interval).await })
}

async fn run(
    registry: VoiceSessionRegistry,
    runtime: VoiceRuntime,
    stop: Arc<Notify>,
    interval: Duration,
) {
    debug!("voice_reaper: starting (interval = {:?})", interval);
    loop {
        tokio::select! {
            _ = stop.notified() => {
                debug!("voice_reaper: stop signal received");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                let reaped = registry.reap_stale().await;
                if !reaped.is_empty() {
                    debug!(
                        "voice_reaper: reaped {} stale session(s): {:?}",
                        reaped.len(),
                        reaped
                    );
                    // Drop the matching runtime state so no orphan
                    // STT/TTS clients linger (their reqwest pools
                    // hold sockets open). `drop_session` is a no-op
                    // for sessions that never reached the runtime
                    // (e.g. registry-only sessions whose first
                    // chunk had an unsupported codec).
                    for session_id in &reaped {
                        runtime.drop_session(session_id).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventBus, UiEvent};
    use crate::voice_frame::VoiceFrameHeader;
    use execlaw_voice_pipeline::traits::{MockStt, MockTts, TtsClient};

    fn header(session: &str) -> VoiceFrameHeader {
        VoiceFrameHeader {
            session: session.into(),
            seq: 0,
            codec: "pcm16le".into(),
            sample_rate: 16_000,
            channels: 1,
            ts_ms: None,
        }
    }

    fn mock_runtime(bus: EventBus) -> VoiceRuntime {
        VoiceRuntime::new(
            bus,
            Arc::new(|| Box::new(MockStt::new(Vec::new(), String::new()))),
            Arc::new(|| (Box::new(MockTts::default()) as Box<dyn TtsClient>, None)),
        )
    }

    #[tokio::test]
    async fn reaper_loop_actually_invokes_reap_stale() {
        // Audit closure — the previous version of this test bypassed
        // the loop entirely. This drives the spawn_with_interval
        // path: forge a stale session, run the reaper with a tiny
        // interval, and assert the loop emits the reap event.
        use crate::voice_session::SESSION_IDLE_TIMEOUT;
        use std::time::Instant;
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let registry = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);

        // Open + age a session into the deep past.
        registry.observe_frame(&header("ghost"), b"a").await;
        {
            // The reach-in here mirrors voice_session::tests; we
            // need the same trick to test the reaper without
            // sleeping for SESSION_IDLE_TIMEOUT.
            let mut inner = registry.private_inner_for_test().await;
            if let Some(s) = inner.sessions_mut().get_mut("ghost") {
                s.set_last_seen_for_test(
                    Instant::now() - SESSION_IDLE_TIMEOUT - Duration::from_secs(1),
                );
            }
        }

        let stop = Arc::new(Notify::new());
        let h = spawn_with_interval(
            registry.clone(),
            runtime.clone(),
            stop.clone(),
            Duration::from_millis(20),
        );

        let mut saw = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(UiEvent::VoiceSessionEnded { session, reason }))
                    if session == "ghost" && reason == "stale_reap" =>
                {
                    saw = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => continue,
            }
        }
        stop.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
        assert!(
            saw,
            "reaper loop must invoke reap_stale and publish VoiceSessionEnded"
        );
    }

    #[tokio::test]
    async fn reaper_drops_runtime_state_for_reaped_sessions() {
        // The audit-closure fix wires reap_stale's returned ids
        // into runtime.drop_session. Verify both maps stay in sync.
        use crate::voice_session::SESSION_IDLE_TIMEOUT;
        use std::time::Instant;
        let bus = EventBus::new();
        let registry = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);

        // Get a session into the runtime by ingesting one chunk,
        // then forge it stale on the registry side.
        registry.observe_frame(&header("orphan"), b"a").await;
        runtime
            .ingest_chunks(&[crate::voice_session::OrderedAudioChunk {
                session: "orphan".into(),
                seq: 0,
                codec: "pcm16le".into(),
                sample_rate: 16_000,
                channels: 1,
                payload: vec![0u8; 32],
            }])
            .await;
        assert_eq!(runtime.live_count().await, 1);

        {
            let mut inner = registry.private_inner_for_test().await;
            if let Some(s) = inner.sessions_mut().get_mut("orphan") {
                s.set_last_seen_for_test(
                    Instant::now() - SESSION_IDLE_TIMEOUT - Duration::from_secs(1),
                );
            }
        }

        let stop = Arc::new(Notify::new());
        let h = spawn_with_interval(
            registry,
            runtime.clone(),
            stop.clone(),
            Duration::from_millis(20),
        );
        // Give the reaper time to fire at least once.
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
        assert_eq!(
            runtime.live_count().await,
            0,
            "reaper must drop runtime state for reaped sessions"
        );
    }

    #[tokio::test]
    async fn reaper_stops_on_notify() {
        let bus = EventBus::new();
        let registry = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);
        let stop = Arc::new(Notify::new());
        let h = spawn(registry, runtime, stop.clone());
        // Fire stop right away — task should exit.
        stop.notify_one();
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("reaper must stop within 2s")
            .expect("reaper task panicked");
    }

    #[tokio::test]
    async fn reap_interval_constant_is_under_session_timeout() {
        // Defensive: if someone bumps SESSION_IDLE_TIMEOUT down to
        // below DEFAULT_REAP_INTERVAL the reaper would miss
        // sessions entirely. Pin the relationship.
        use crate::voice_session::SESSION_IDLE_TIMEOUT;
        assert!(
            DEFAULT_REAP_INTERVAL < SESSION_IDLE_TIMEOUT,
            "DEFAULT_REAP_INTERVAL ({:?}) must be < SESSION_IDLE_TIMEOUT ({:?})",
            DEFAULT_REAP_INTERVAL,
            SESSION_IDLE_TIMEOUT
        );
    }
}
