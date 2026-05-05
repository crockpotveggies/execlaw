//! Phase 13.C — `WhisperClient` HTTP adapter.
//!
//! Buffers PCM16 chunks across `push` calls; on `flush` POSTs the
//! accumulated buffer to faster-whisper's OpenAI-compat endpoint
//! (`POST /v1/audio/transcriptions` with multipart form field
//! `file=` containing a WAV blob and `model=whisper-1`). Returns
//! the transcript via `SttEvent::Final`.
//!
//! Partial transcripts are not surfaced by the OpenAI-compat
//! endpoint — faster-whisper exposes a streaming WebSocket route in
//! upstream forks but we'd need a second adapter for that. For v1
//! the user sees the partial as "still listening…" and the final
//! arrives on flush. A streaming adapter can drop in behind the
//! same trait when it lands.

use async_trait::async_trait;
use execlaw_voice_pipeline::traits::{AudioChunk, SttClient, SttEvent};
use std::time::Duration;

/// Sample rate the WAV header encodes. Whisper expects 16 kHz mono.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

/// HTTP client around faster-whisper's `/v1/audio/transcriptions`.
pub struct WhisperClient {
    base_url: String,
    client: reqwest::Client,
    /// PCM16 LE samples accumulated across push() calls. Flushed on
    /// `flush()`. We don't enforce a max length here — the caller
    /// (voice_runtime) endpoint-detects via VAD and flushes well
    /// under the ~30s practical cap.
    buffer: Vec<i16>,
    /// Optional model name to pass — defaults to "whisper-1" which
    /// the OpenAI-compat shim accepts as a wildcard.
    model: String,
    request_timeout: Duration,
}

impl WhisperClient {
    /// `base_url` is e.g. `http://127.0.0.1:8001` — the supervisor-
    /// resolved endpoint for the VoiceSTT backend row.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_client(base_url, reqwest::Client::new())
    }

    /// Test-friendly constructor: lets callers pass a pre-configured
    /// `reqwest::Client` (e.g. with custom redirect policy or
    /// connect_timeout).
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
            buffer: Vec::new(),
            model: "whisper-1".to_owned(),
            // Whisper inference is bounded by audio length; 30s
            // upload + decode is plenty for the VAD-segmented chunks
            // the pipeline produces (typically <10s).
            request_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Build a 44-byte WAV header for `n_samples` of PCM16 mono at
    /// 16 kHz, then concatenate the samples. faster-whisper accepts
    /// raw PCM but the OpenAI-compat shim insists on a real audio
    /// container; WAV is the cheapest one to produce in Rust without
    /// pulling in an encoder crate.
    fn pcm_to_wav(samples: &[i16]) -> Vec<u8> {
        let n_samples = samples.len() as u32;
        let data_size = n_samples * 2; // bytes per sample, mono
        let mut out = Vec::with_capacity(44 + data_size as usize);
        // RIFF header.
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_size).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        // "fmt " sub-chunk.
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM audio format
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&WHISPER_SAMPLE_RATE.to_le_bytes());
        let byte_rate = WHISPER_SAMPLE_RATE * 2;
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes()); // block align (mono * 2 bytes)
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        // "data" sub-chunk.
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// POST the buffered samples to the Whisper endpoint and parse
    /// the resulting transcript.
    async fn post_transcribe(&self, samples: &[i16]) -> Result<String, String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let wav = Self::pcm_to_wav(samples);
        let url = format!("{}/v1/audio/transcriptions", self.base_url);
        let part = reqwest::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| format!("multipart wav part: {e}"))?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", part);
        let resp = self
            .client
            .post(&url)
            .timeout(self.request_timeout)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("whisper POST {url} failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("whisper read body: {e}"))?;
        if !status.is_success() {
            return Err(format!("whisper {status}: {body}"));
        }
        // OpenAI-compat returns either {"text":"…"} JSON or plain
        // text — depends on the requested response_format. We don't
        // pass response_format so the default is JSON.
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("whisper response not JSON ({body:?}): {e}"))?;
        let text = parsed
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        Ok(text)
    }

    /// Test-only: how many samples are currently buffered.
    #[cfg(test)]
    pub fn buffered_samples(&self) -> usize {
        self.buffer.len()
    }
}

#[async_trait]
impl SttClient for WhisperClient {
    async fn push(&mut self, chunk: &AudioChunk) {
        self.buffer.extend_from_slice(&chunk.samples);
    }

    async fn flush(&mut self) -> SttEvent {
        let samples = std::mem::take(&mut self.buffer);
        match self.post_transcribe(&samples).await {
            Ok(text) => SttEvent::Final { text },
            Err(e) => {
                tracing::warn!("whisper flush failed: {e}");
                // Surface as an empty Final so the caller can
                // commit the empty turn and keep the conversation
                // alive instead of locking up.
                SttEvent::Final {
                    text: String::new(),
                }
            }
        }
    }

    async fn peek_partial(&self) -> Option<String> {
        // The OpenAI-compat endpoint isn't streaming, so there's no
        // running partial to peek at. Returning None tells the
        // pipeline "no preview yet" and it falls back to the
        // typing-indicator UX.
        None
    }

    async fn reset(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawns a tiny HTTP fixture on an ephemeral port that responds
    /// to any POST with `body_factory(call_count)`. Returns the
    /// base URL and a counter the test asserts on.
    async fn spawn_http_fixture(
        body_factory: Arc<dyn Fn(usize) -> (u16, String) + Send + Sync>,
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
                    // Drain the request — we don't validate the
                    // body shape in these unit tests; that's the
                    // job of an integration test.
                    let mut buf = vec![0u8; 64 * 1024];
                    let _ = stream.read(&mut buf).await;
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = body_factory(n);
                    let resp = format!(
                        "HTTP/1.1 {} OK\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {}",
                        status,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), counter)
    }

    fn one_chunk(n_samples: usize) -> AudioChunk {
        AudioChunk {
            start_ms: 0,
            end_ms: 1_000,
            samples: vec![1234i16; n_samples],
        }
    }

    #[test]
    fn pcm_to_wav_emits_44_byte_header_and_pcm_payload() {
        let samples = vec![1i16, -1, 32_767, -32_768];
        let wav = WhisperClient::pcm_to_wav(&samples);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // 44-byte header + 4 samples * 2 bytes.
        assert_eq!(wav.len(), 44 + 8);
        // Last 8 bytes are the PCM payload, little-endian.
        assert_eq!(&wav[44..46], &1i16.to_le_bytes());
        assert_eq!(&wav[46..48], &(-1i16).to_le_bytes());
    }

    #[test]
    fn pcm_to_wav_carries_16khz_sample_rate() {
        let wav = WhisperClient::pcm_to_wav(&[0i16; 4]);
        // Bytes 24..28 are sample-rate LE u32.
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&wav[24..28]);
        assert_eq!(u32::from_le_bytes(buf), 16_000);
    }

    #[tokio::test]
    async fn push_then_flush_round_trips_via_http_fixture() {
        let (base, calls) =
            spawn_http_fixture(Arc::new(|_n| (200, r#"{"text":"hello world"}"#.to_owned()))).await;
        let mut client = WhisperClient::new(base);
        client.push(&one_chunk(160)).await; // ~10ms at 16kHz
        client.push(&one_chunk(160)).await;
        match client.flush().await {
            SttEvent::Final { text } => assert_eq!(text, "hello world"),
            other => panic!("expected Final, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn flush_with_empty_buffer_returns_empty_final_without_http_call() {
        // Callers can flush defensively; we shouldn't spam the
        // backend with empty WAV uploads.
        let (base, calls) = spawn_http_fixture(Arc::new(|_n| {
            (200, r#"{"text":"won't be hit"}"#.to_owned())
        }))
        .await;
        let mut client = WhisperClient::new(base);
        match client.flush().await {
            SttEvent::Final { text } => assert!(text.is_empty()),
            other => panic!("expected Final, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn flush_swallows_5xx_as_empty_final() {
        // A transient backend failure shouldn't tear down the
        // conversation. The empty Final lets the pipeline commit a
        // null turn and keep going.
        let (base, _calls) = spawn_http_fixture(Arc::new(|_n| {
            (500, r#"{"error":"out of memory"}"#.to_owned())
        }))
        .await;
        let mut client = WhisperClient::new(base);
        client.push(&one_chunk(320)).await;
        let ev = client.flush().await;
        match ev {
            SttEvent::Final { text } => assert!(text.is_empty()),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_clears_buffer_without_calling_http() {
        let (base, calls) =
            spawn_http_fixture(Arc::new(|_n| (200, r#"{"text":"won't run"}"#.to_owned()))).await;
        let mut client = WhisperClient::new(base);
        client.push(&one_chunk(640)).await;
        assert_eq!(client.buffered_samples(), 640);
        client.reset().await;
        assert_eq!(client.buffered_samples(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn peek_partial_is_none_for_v1_openai_compat() {
        let client = WhisperClient::new("http://127.0.0.1:1");
        assert!(client.peek_partial().await.is_none());
    }

    #[tokio::test]
    async fn flush_handles_malformed_json_gracefully() {
        // OpenAI-compat shims sometimes return text/plain when
        // misconfigured; our parser should treat non-JSON as an
        // empty final rather than panicking.
        let (base, _) =
            spawn_http_fixture(Arc::new(|_n| (200, "not json at all".to_owned()))).await;
        let mut client = WhisperClient::new(base);
        client.push(&one_chunk(160)).await;
        match client.flush().await {
            SttEvent::Final { text } => assert!(text.is_empty()),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn flush_trims_whitespace_around_text() {
        let (base, _) =
            spawn_http_fixture(Arc::new(|_n| (200, r#"{"text":"  hello  "}"#.to_owned()))).await;
        let mut client = WhisperClient::new(base);
        client.push(&one_chunk(160)).await;
        match client.flush().await {
            SttEvent::Final { text } => assert_eq!(text, "hello"),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    /// Audit closure: the previous fixtures only exercised the HTTP
    /// happy path. This one captures the request bytes and asserts
    /// the wire shape — multipart Content-Type, the `model=` form
    /// field, and the WAV header on the audio part.
    #[tokio::test]
    async fn flush_sends_openai_compat_multipart_with_model_field() {
        use std::sync::Mutex as StdMutex;
        let captured: Arc<StdMutex<Vec<u8>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 256 * 1024];
            // Read until we see end-of-headers or fill the buffer.
            // 256 KiB is plenty for a one-second WAV at 16 kHz.
            let mut total = 0;
            while total < buf.len() {
                let n = match stream.read(&mut buf[total..]).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                total += n;
                // Heuristic: stop once we've seen the "data" WAV
                // marker, which is well past the headers + multipart
                // boundary.
                if buf[..total].windows(4).any(|w| w == b"data") {
                    break;
                }
            }
            captured_clone
                .lock()
                .unwrap()
                .extend_from_slice(&buf[..total]);
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"text\":\"ok\"}";
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        let base = format!("http://127.0.0.1:{port}");
        let mut client = WhisperClient::new(base);
        client.push(&one_chunk(640)).await;
        let _ = client.flush().await;
        let bytes = captured.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&bytes);
        let s_lower = s.to_ascii_lowercase();
        assert!(
            s_lower.contains("content-type: multipart/form-data"),
            "request must be multipart/form-data; got headers: {s:?}"
        );
        assert!(
            s.contains("name=\"model\""),
            "request must carry the model form field"
        );
        assert!(
            s.contains("name=\"file\""),
            "request must carry the file form field"
        );
        // The audio part must be a real WAV — RIFF + WAVE markers
        // should appear in the body.
        assert!(bytes.windows(4).any(|w| w == b"RIFF"));
        assert!(bytes.windows(4).any(|w| w == b"WAVE"));
    }
}
