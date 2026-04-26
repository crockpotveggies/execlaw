//! Phase 13.C — `KokoroClient` HTTP adapter.
//!
//! Calls Kokoro-FastAPI's OpenAI-compat speech endpoint
//! (`POST /v1/audio/speech` with JSON `{"input": "...", "voice": "..."}`).
//! Response is raw PCM16 little-endian mono at the rate the server
//! configured (typically 24 kHz for Kokoro). The client returns a
//! `TtsAudio` carrying those samples; the SPA-side player does the
//! resample-to-output-rate dance because every browser's
//! `AudioContext` has its own native rate.
//!
//! Cancellation: `cancel()` flips an internal flag that the
//! in-flight `synthesize()` checks before parsing the response.
//! Once an HTTP request is on the wire `reqwest` doesn't expose a
//! cheap kill, so we let it finish and discard the body. For real
//! barge-in the SPA-side `VoicePlayback` flush is what the user
//! perceives; the server-side cancel just stops the bytes from
//! being broadcast.

use async_trait::async_trait;
use execlaw_voice_pipeline::traits::{TtsAudio, TtsClient};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Sample rate Kokoro emits. The SPA rescales as needed.
pub const KOKORO_SAMPLE_RATE: u32 = 24_000;

/// HTTP client around Kokoro-FastAPI's `/v1/audio/speech`.
pub struct KokoroClient {
    base_url: String,
    client: reqwest::Client,
    voice_id: String,
    request_timeout: Duration,
    /// Bumped on every `cancel()`. If a `synthesize()` started before
    /// the cancel and finishes after, the bumped counter signals it
    /// to drop the result.
    cancel_epoch: Arc<AtomicUsize>,
}

impl KokoroClient {
    /// `voice_id` follows Kokoro's combined-voice syntax (e.g.
    /// `bf_emma+am_michael` for the locked-decision blend).
    pub fn new(base_url: impl Into<String>, voice_id: impl Into<String>) -> Self {
        Self::with_client(base_url, voice_id, reqwest::Client::new())
    }

    pub fn with_client(
        base_url: impl Into<String>,
        voice_id: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
            voice_id: voice_id.into(),
            request_timeout: Duration::from_secs(20),
            cancel_epoch: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Convenience for callers that learn the voice id from
    /// Settings → Personality after the client was constructed.
    pub fn set_voice_id(&mut self, voice_id: impl Into<String>) {
        self.voice_id = voice_id.into();
    }

    /// Returns a cheap, cloneable handle the caller can use to
    /// cancel an in-flight synthesize from another task. The
    /// synthesize task holds the client (`&mut self`) so a sibling
    /// task can't reach `cancel()` directly — this handle is the
    /// supported escape hatch (used by `voice_runtime` when a
    /// `voice_interrupt` WS message lands during a synthesize).
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle {
            epoch: self.cancel_epoch.clone(),
        }
    }

    /// Test-only: how many times `cancel()` was observed.
    #[cfg(test)]
    pub fn cancel_count(&self) -> usize {
        self.cancel_epoch.load(Ordering::SeqCst)
    }

    /// Parse a raw PCM16 LE byte stream into i16 samples. We trust
    /// the backend's content-type — the OpenAI-compat shim returns
    /// `audio/pcm` for `response_format: pcm`, which is what
    /// Kokoro-FastAPI's docs call out.
    fn parse_pcm16_le(bytes: &[u8]) -> Vec<i16> {
        let mut out = Vec::with_capacity(bytes.len() / 2);
        let mut i = 0;
        while i + 1 < bytes.len() {
            let lo = bytes[i];
            let hi = bytes[i + 1];
            out.push(i16::from_le_bytes([lo, hi]));
            i += 2;
        }
        out
    }
}

/// Cancellation handle returned by `KokoroClient::interrupt_handle`.
/// Cheaply cloneable; bump `fire()` from any task to interrupt the
/// matching client's in-flight `synthesize()`. The synthesize
/// observes the bump on completion and drops the audio bytes.
#[derive(Debug, Clone)]
pub struct InterruptHandle {
    epoch: Arc<AtomicUsize>,
}

impl InterruptHandle {
    /// Bump the cancel epoch. The next `synthesize()` boundary
    /// check on the matching client will see the bump and discard
    /// the result.
    pub fn fire(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Read the current epoch. Callers (e.g. `voice_runtime`) snap
    /// this before a long-running operation and compare after to
    /// detect a fire that happened during the wait. Cheap atomic
    /// load.
    pub fn epoch_value(&self) -> usize {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Test-only alias matching the older name. Keep both so
    /// existing tests stay readable.
    #[cfg(test)]
    pub fn epoch(&self) -> usize {
        self.epoch_value()
    }
}

#[async_trait]
impl TtsClient for KokoroClient {
    async fn synthesize(&mut self, text: &str) -> Result<TtsAudio, String> {
        if text.trim().is_empty() {
            return Ok(TtsAudio {
                text: text.to_owned(),
                samples: Vec::new(),
                duration_ms: 0,
            });
        }
        let cancel_at_start = self.cancel_epoch.load(Ordering::SeqCst);
        let url = format!("{}/v1/audio/speech", self.base_url);
        let body = serde_json::json!({
            "model": "kokoro",
            "voice": self.voice_id,
            "input": text,
            // PCM16 lets the SPA side decode without an audio crate.
            "response_format": "pcm",
        });
        let resp = self
            .client
            .post(&url)
            .timeout(self.request_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("kokoro POST {url} failed: {e}"))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("kokoro read body: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "kokoro {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        // Drop the result if the operator interrupted while the
        // request was on the wire.
        if self.cancel_epoch.load(Ordering::SeqCst) != cancel_at_start {
            return Ok(TtsAudio {
                text: text.to_owned(),
                samples: Vec::new(),
                duration_ms: 0,
            });
        }
        let samples = Self::parse_pcm16_le(&bytes);
        let duration_ms = (samples.len() as u64 * 1000 / KOKORO_SAMPLE_RATE as u64) as u32;
        Ok(TtsAudio {
            text: text.to_owned(),
            samples,
            duration_ms,
        })
    }

    async fn cancel(&mut self) {
        self.cancel_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type FixtureBodyFactory =
        Arc<dyn Fn(usize) -> (u16, &'static str, Vec<u8>) + Send + Sync>;

    /// Fixture HTTP server that responds with raw bytes (so we can
    /// return PCM blobs alongside the OpenAI-compat error JSON).
    /// `body_factory` returns `(status, content_type, body_bytes)`.
    async fn spawn_pcm_fixture(
        body_factory: FixtureBodyFactory,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_handle = counter.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter_handle.clone();
                let body_factory = body_factory.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 * 1024];
                    let _ = stream.read(&mut buf).await;
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let (status, ct, body) = body_factory(n);
                    let head = format!(
                        "HTTP/1.1 {} OK\r\n\
                         Content-Type: {}\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n",
                        status,
                        ct,
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), counter)
    }

    fn pcm_blob(samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn parse_pcm16_le_round_trips_samples() {
        let samples = vec![0i16, 1, -1, 32_767, -32_768, 12_345];
        let bytes = pcm_blob(&samples);
        let parsed = KokoroClient::parse_pcm16_le(&bytes);
        assert_eq!(parsed, samples);
    }

    #[test]
    fn parse_pcm16_le_drops_trailing_odd_byte() {
        // A misbehaving server returning an odd-length body must not
        // panic the client — we drop the dangling byte.
        let mut bytes = pcm_blob(&[1i16, 2]);
        bytes.push(0xff);
        let parsed = KokoroClient::parse_pcm16_le(&bytes);
        assert_eq!(parsed, vec![1, 2]);
    }

    #[tokio::test]
    async fn synthesize_returns_samples_from_pcm_response() {
        let pcm = pcm_blob(&[100i16, 200, -300]);
        let pcm_clone = pcm.clone();
        let (base, calls) = spawn_pcm_fixture(Arc::new(move |_n| {
            (200, "audio/pcm", pcm_clone.clone())
        }))
        .await;
        let mut client = KokoroClient::new(base, "bf_emma+am_michael");
        let audio = client.synthesize("hello").await.unwrap();
        assert_eq!(audio.text, "hello");
        assert_eq!(audio.samples, vec![100i16, 200, -300]);
        // 3 samples / 24000 Hz * 1000 = 0ms — duration math drops to
        // zero for sub-sample-rate-resolution chunks. That's fine;
        // the SPA only uses duration_ms for log breadcrumbs.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn synthesize_empty_text_short_circuits_without_http() {
        let pcm = pcm_blob(&[1i16]);
        let (base, calls) = spawn_pcm_fixture(Arc::new(move |_n| {
            (200, "audio/pcm", pcm.clone())
        }))
        .await;
        let mut client = KokoroClient::new(base, "bf_emma");
        let audio = client.synthesize("   ").await.unwrap();
        assert!(audio.samples.is_empty());
        assert_eq!(audio.duration_ms, 0);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "empty text must not hit the backend"
        );
    }

    #[tokio::test]
    async fn synthesize_returns_err_on_5xx() {
        let body = b"{\"error\":\"oom\"}".to_vec();
        let (base, _) = spawn_pcm_fixture(Arc::new(move |_n| {
            (500, "application/json", body.clone())
        }))
        .await;
        let mut client = KokoroClient::new(base, "bf_emma");
        let r = client.synthesize("hello").await;
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("kokoro"));
        assert!(msg.contains("500"));
    }

    #[tokio::test]
    async fn interrupt_handle_fired_during_synthesize_drops_audio() {
        // The fixture delays before responding so the test has time
        // to fire the InterruptHandle while the request is in
        // flight. The client snapshots the epoch at the start of
        // synthesize() and compares after the body is read; a bump
        // in between means "operator interrupted" and the audio is
        // dropped.
        let pcm = pcm_blob(&[1i16, 2, 3, 4]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let _ = stream.read(&mut buf).await;
            // Hold the response open for ~150ms so the test thread
            // has time to bump the cancel epoch.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/pcm\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                pcm.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&pcm).await;
            let _ = stream.shutdown().await;
        });
        let base = format!("http://127.0.0.1:{port}");
        let mut client = KokoroClient::new(base, "bf_emma");
        let handle = client.interrupt_handle();
        // Fire the handle ~50ms in — while synthesize is still
        // awaiting the fixture's 150ms delay.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            handle.fire();
        });
        let audio = client.synthesize("hi").await.unwrap();
        assert!(
            audio.samples.is_empty(),
            "post-interrupt synthesize must drop the audio"
        );
    }

    #[tokio::test]
    async fn interrupt_handle_fired_after_synthesize_does_not_drop_audio() {
        // Inverse of above — a fire that lands AFTER synthesize
        // completes must not retroactively zero out the audio.
        let pcm = pcm_blob(&[100i16, 200]);
        let pcm_clone = pcm.clone();
        let (base, _) = spawn_pcm_fixture(Arc::new(move |_n| {
            (200, "audio/pcm", pcm_clone.clone())
        }))
        .await;
        let mut client = KokoroClient::new(base, "bf_emma");
        let audio = client.synthesize("hi").await.unwrap();
        assert_eq!(audio.samples, vec![100i16, 200]);
        // Late fire — no effect on already-returned audio.
        client.interrupt_handle().fire();
        assert_eq!(audio.samples, vec![100i16, 200]);
    }

    #[tokio::test]
    async fn cancel_count_increments_per_call() {
        let mut client = KokoroClient::new("http://127.0.0.1:1", "bf_emma");
        client.cancel().await;
        client.cancel().await;
        assert_eq!(client.cancel_count(), 2);
    }

    #[test]
    fn set_voice_id_replaces_blend() {
        let mut client = KokoroClient::new("http://127.0.0.1:1", "bf_emma");
        client.set_voice_id("af_sarah+am_adam");
        assert_eq!(client.voice_id, "af_sarah+am_adam");
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let client = KokoroClient::new("http://127.0.0.1:1234/", "bf_emma");
        assert_eq!(client.base_url, "http://127.0.0.1:1234");
    }

    /// Audit closure: assert the request carries the OpenAI-compat
    /// JSON shape — `voice`, `input`, and `response_format: pcm`.
    /// A regression that drops one of these would otherwise pass
    /// every other test.
    #[tokio::test]
    async fn synthesize_sends_voice_input_and_pcm_format() {
        use std::sync::Mutex as StdMutex;
        let captured: Arc<StdMutex<Vec<u8>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let pcm = pcm_blob(&[1i16, 2]);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let mut total = 0;
            while total < buf.len() {
                let n = match stream.read(&mut buf[total..]).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                total += n;
                // Stop after we see the JSON body's closing brace.
                if buf[..total].iter().filter(|&&b| b == b'}').count() >= 1 {
                    break;
                }
            }
            captured_clone.lock().unwrap().extend_from_slice(&buf[..total]);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/pcm\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                pcm.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&pcm).await;
            let _ = stream.shutdown().await;
        });
        let base = format!("http://127.0.0.1:{port}");
        let mut client = KokoroClient::new(base, "bf_emma+am_michael");
        let _ = client.synthesize("hello voice").await;
        let bytes = captured.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&bytes);
        let s_lower = s.to_ascii_lowercase();
        // Path must be the OpenAI-compat speech endpoint.
        assert!(
            s.contains("POST /v1/audio/speech"),
            "request path must be /v1/audio/speech; got {s:?}"
        );
        assert!(
            s_lower.contains("content-type: application/json"),
            "request must be JSON; got headers: {s:?}"
        );
        // JSON body fields. We don't parse — substring matches are
        // robust against future field additions.
        assert!(
            s.contains("\"voice\":\"bf_emma+am_michael\""),
            "voice field missing or wrong"
        );
        assert!(s.contains("\"input\":\"hello voice\""));
        assert!(
            s.contains("\"response_format\":\"pcm\""),
            "must request pcm; SPA-side decoder doesn't speak other codecs"
        );
    }
}
