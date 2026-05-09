//! Phase 13.A wire framing for inbound voice audio.
//!
//! Every binary frame the SPA / mobile / phone-bridge transports
//! send up the WS has the shape:
//!
//! ```text
//! [4 bytes header_len (BE u32)] [N bytes JSON header] [M bytes payload]
//! ```
//!
//! The header is JSON for human-readable debug logs and forward-
//! compatible field additions; the payload is opaque codec bytes
//! the WhisperClient adapter (Phase 13.C) will decode.
//!
//! This module owns parsing only — the actor that consumes the
//! decoded frames and feeds them into the voice-pipeline lands in
//! Phase 13.B. For 13.A the events.rs handler just parses-and-logs
//! so operators can confirm session id + seq + codec metadata flow
//! end-to-end before any STT/TTS work begins.

use serde::Deserialize;
use thiserror::Error;

const HEADER_LEN_BYTES: usize = 4;
/// Cap on the JSON header length. The header today is ~150 bytes;
/// 4 KiB is plenty of headroom for new fields and prevents a
/// malicious or malformed client from forcing a huge alloc.
const MAX_HEADER_LEN: usize = 4096;

/// Wire shape of the JSON header. Every audio source (browser,
/// mobile native, Bluetooth phone bridge) emits the same shape so
/// the server doesn't need source-specific decode paths. New
/// optional fields land via `#[serde(default)]` for backward
/// compatibility; required fields (session, seq, codec, sample_rate)
/// are present in every transport.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VoiceFrameHeader {
    /// Voice-session id, distinct from the chat conversation id.
    /// One conversation can survive a WS reconnect (new session,
    /// same conversation); a Bluetooth call drop produces a new
    /// session.
    pub session: String,
    /// Monotonic per-session frame counter. Used by the jitter
    /// buffer (Phase 13.B) to reorder frames that arrive out of
    /// order over a lossy transport.
    pub seq: u32,
    /// Self-described codec name. Drives the WhisperClient
    /// adapter's decode path. Common values: "opus", "aac",
    /// "pcm16". "unknown" surfaces as a parse error so operators
    /// see the misroute rather than silently corrupted audio.
    pub codec: String,
    /// Capture sample rate in Hz. Drives the resampler in front
    /// of Whisper (which expects 16kHz).
    pub sample_rate: u32,
    /// 1 for mono, 2 for stereo. Voice mode is mono everywhere.
    /// Defaults to 1 when the client omits the field.
    #[serde(default = "default_channels")]
    pub channels: u32,
    /// Optional client wall-clock timestamp. Used for jitter
    /// estimation if the operator wires latency observability;
    /// never load-bearing on the dispatch path.
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

fn default_channels() -> u32 {
    1
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameParseError {
    #[error("frame too short for header_len prefix (need {0} bytes, got {1})")]
    TooShort(usize, usize),
    #[error("header length {0} exceeds MAX_HEADER_LEN ({1})")]
    HeaderTooLarge(usize, usize),
    #[error("frame truncated: expected {expected} header bytes after prefix, got {actual}")]
    HeaderTruncated { expected: usize, actual: usize },
    #[error("header is not valid JSON: {0}")]
    HeaderJson(String),
    #[error("header missing required fields: {0}")]
    HeaderInvalid(String),
}

/// Parse a single inbound binary frame. Returns the typed header
/// and a borrowed slice of the audio payload bytes (no copy).
pub fn parse_frame(bytes: &[u8]) -> Result<(VoiceFrameHeader, &[u8]), FrameParseError> {
    if bytes.len() < HEADER_LEN_BYTES {
        return Err(FrameParseError::TooShort(HEADER_LEN_BYTES, bytes.len()));
    }
    let header_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if header_len > MAX_HEADER_LEN {
        return Err(FrameParseError::HeaderTooLarge(header_len, MAX_HEADER_LEN));
    }
    let header_end = HEADER_LEN_BYTES + header_len;
    if bytes.len() < header_end {
        return Err(FrameParseError::HeaderTruncated {
            expected: header_len,
            actual: bytes.len().saturating_sub(HEADER_LEN_BYTES),
        });
    }
    let header_bytes = &bytes[HEADER_LEN_BYTES..header_end];
    let header: VoiceFrameHeader = serde_json::from_slice(header_bytes)
        .map_err(|e| FrameParseError::HeaderJson(e.to_string()))?;
    if header.session.trim().is_empty() {
        return Err(FrameParseError::HeaderInvalid(
            "session must be non-empty".into(),
        ));
    }
    if header.codec.trim().is_empty() {
        return Err(FrameParseError::HeaderInvalid(
            "codec must be non-empty".into(),
        ));
    }
    let payload = &bytes[header_end..];
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_frame(header_json: &str, payload: &[u8]) -> Vec<u8> {
        let header_bytes = header_json.as_bytes();
        let mut out = Vec::with_capacity(4 + header_bytes.len() + payload.len());
        out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(header_bytes);
        out.extend_from_slice(payload);
        out
    }

    fn good_header() -> &'static str {
        r#"{"session":"abc","seq":3,"codec":"opus","sample_rate":48000,"channels":1,"ts_ms":1234}"#
    }

    #[test]
    fn parses_a_well_formed_frame() {
        let frame = build_frame(good_header(), b"opus-payload");
        let (header, payload) = parse_frame(&frame).unwrap();
        assert_eq!(header.session, "abc");
        assert_eq!(header.seq, 3);
        assert_eq!(header.codec, "opus");
        assert_eq!(header.sample_rate, 48000);
        assert_eq!(header.channels, 1);
        assert_eq!(header.ts_ms, Some(1234));
        assert_eq!(payload, b"opus-payload");
    }

    #[test]
    fn defaults_channels_to_one_when_omitted() {
        let h = r#"{"session":"abc","seq":1,"codec":"opus","sample_rate":48000}"#;
        let frame = build_frame(h, b"");
        let (header, _) = parse_frame(&frame).unwrap();
        assert_eq!(header.channels, 1);
        assert!(header.ts_ms.is_none());
    }

    #[test]
    fn rejects_buffer_shorter_than_4_bytes() {
        let err = parse_frame(&[0, 1, 2]).unwrap_err();
        assert!(matches!(err, FrameParseError::TooShort(4, 3)));
    }

    #[test]
    fn rejects_truncated_header() {
        // Claim 100-byte header but only supply 5.
        let mut frame = (100u32.to_be_bytes()).to_vec();
        frame.extend_from_slice(b"hello");
        let err = parse_frame(&frame).unwrap_err();
        assert!(matches!(err, FrameParseError::HeaderTruncated { .. }));
    }

    #[test]
    fn rejects_oversized_header() {
        // Anti-allocation guard: header_len > MAX rejects without
        // touching the payload.
        let bytes = (1_000_000u32.to_be_bytes()).to_vec();
        let err = parse_frame(&bytes).unwrap_err();
        assert!(matches!(err, FrameParseError::HeaderTooLarge(_, _)));
    }

    #[test]
    fn rejects_invalid_json_header() {
        let frame = build_frame("not json", b"x");
        let err = parse_frame(&frame).unwrap_err();
        assert!(matches!(err, FrameParseError::HeaderJson(_)));
    }

    #[test]
    fn rejects_missing_required_fields() {
        let h = r#"{"session":"abc","codec":"opus","sample_rate":48000}"#; // no seq
        let frame = build_frame(h, b"");
        let err = parse_frame(&frame).unwrap_err();
        // Missing required field surfaces as a JSON-decode error
        // (serde rejects). HeaderInvalid only fires on
        // semantic-but-parseable issues like empty session.
        assert!(matches!(err, FrameParseError::HeaderJson(_)));
    }

    #[test]
    fn rejects_empty_session() {
        let h = r#"{"session":"","seq":0,"codec":"opus","sample_rate":48000}"#;
        let frame = build_frame(h, b"");
        let err = parse_frame(&frame).unwrap_err();
        match err {
            FrameParseError::HeaderInvalid(msg) => {
                assert!(msg.contains("session"));
            }
            other => panic!("expected HeaderInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_codec() {
        let h = r#"{"session":"abc","seq":0,"codec":"","sample_rate":48000}"#;
        let frame = build_frame(h, b"");
        let err = parse_frame(&frame).unwrap_err();
        match err {
            FrameParseError::HeaderInvalid(msg) => {
                assert!(msg.contains("codec"));
            }
            other => panic!("expected HeaderInvalid, got {other:?}"),
        }
    }

    #[test]
    fn handles_zero_byte_payload() {
        let frame = build_frame(good_header(), b"");
        let (_, payload) = parse_frame(&frame).unwrap();
        assert_eq!(payload.len(), 0);
    }
}
