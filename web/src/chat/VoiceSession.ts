// Phase 13.A closure — voice-session lifecycle on the SPA side.
//
// A `VoiceSession` represents one continuous mic-capture span.
// Each session has a stable `sessionId` (UUID), a monotonic `seq`
// counter, and metadata about the codec + sample rate the source
// is producing. Browser sessions live for the duration of one mic
// click. iOS / Android native and Bluetooth-phone-bridge clients
// will produce their own sessions with the same shape.
//
// The wire format is uniform across every audio source:
//
//   [4 bytes header_len (BE u32)] [N bytes JSON header] [M bytes payload]
//
//   header: {
//     session:     UUID,
//     seq:         u32,
//     codec:       "opus" | "aac" | "pcm16" | ...,
//     sample_rate: 48000 | 16000 | ...,
//     channels:    1,
//     ts_ms:       client wall-clock (optional)
//   }
//
// Why this shape: source-agnostic (browser/mobile/phone), self-
// describing (codec + rate per frame so the server-side decoder
// doesn't need out-of-band setup), session-multiplexable (one WS
// can carry frames from multiple concurrent sessions, e.g. an
// inbound phone call alongside an SPA session).
//
// Reconnect: the SPA side mints a NEW session on every mic click.
// A WS reconnect mid-recording produces a fresh session id; the
// server's stale-session reaper drops the old one after a grace
// window.

const HEADER_LEN_BYTES = 4;

export interface VoiceFrameHeader {
    session: string;
    seq: number;
    codec: string;
    sample_rate: number;
    channels: number;
    ts_ms: number;
}

export interface VoiceSessionOpts {
    /// Codec name like "opus", "aac", "pcm16". Source-specific —
    /// the SPA reads this from MediaRecorder's chosen MIME type.
    codec: string;
    /// Capture sample rate. Browser default is 48000 (Web Audio
    /// AudioContext.sampleRate); phone bridges produce 8000 or
    /// 16000 depending on the codec the gateway transcodes to.
    sampleRate: number;
    /// Optional override for the session id — only used by tests
    /// that need a deterministic UUID. Production always mints a
    /// fresh one via crypto.randomUUID.
    sessionIdOverride?: string;
}

/**
 * One continuous voice-capture session. The class is intentionally
 * tiny — it owns the session id + seq counter, knows the codec
 * metadata, and produces wire-ready framed buffers. It does NOT
 * own the WebSocket or the MediaRecorder; those are wired by the
 * caller (Composer / Chat shell).
 */
export class VoiceSession {
    readonly sessionId: string;
    readonly codec: string;
    readonly sampleRate: number;
    readonly channels: number;
    readonly startMs: number;
    private seq: number = 0;

    constructor(opts: VoiceSessionOpts) {
        this.sessionId = opts.sessionIdOverride ?? mintSessionId();
        this.codec = opts.codec;
        this.sampleRate = opts.sampleRate;
        this.channels = 1;
        this.startMs = Date.now();
    }

    /// Number of frames this session has produced so far. Useful
    /// for tests + debug instrumentation; not exposed on the wire.
    framesSent(): number {
        return this.seq;
    }

    /// Wrap an audio payload with the framing header. Returns a
    /// single ArrayBuffer ready to hand to `WebSocket.send`. Each
    /// call increments the internal `seq` counter — `framesSent()`
    /// reflects how many frames this session has produced.
    framePayload(payload: ArrayBuffer): ArrayBuffer {
        const headerObj: VoiceFrameHeader = {
            session: this.sessionId,
            seq: this.seq,
            codec: this.codec,
            sample_rate: this.sampleRate,
            channels: this.channels,
            ts_ms: Date.now(),
        };
        this.seq += 1;
        const headerJson = JSON.stringify(headerObj);
        const headerBytes = new TextEncoder().encode(headerJson);
        const out = new ArrayBuffer(
            HEADER_LEN_BYTES + headerBytes.byteLength + payload.byteLength,
        );
        const view = new DataView(out);
        view.setUint32(0, headerBytes.byteLength, false); // big-endian
        new Uint8Array(out, HEADER_LEN_BYTES, headerBytes.byteLength).set(
            headerBytes,
        );
        new Uint8Array(
            out,
            HEADER_LEN_BYTES + headerBytes.byteLength,
        ).set(new Uint8Array(payload));
        return out;
    }
}

/**
 * Translate a MediaRecorder MIME type to the canonical codec
 * name used in the wire header. Unrecognized types fall through
 * to "unknown" — the server logs and drops these so the operator
 * can see "we got a frame in a codec we don't decode" rather
 * than silently corrupted audio.
 */
export function codecFromMime(mime: string | undefined): string {
    if (!mime) return "unknown";
    const lower = mime.toLowerCase();
    if (lower.includes("opus")) return "opus";
    // Bare webm/ogg containers ship Opus as the browser default
    // when MediaRecorder isn't given an explicit codec parameter.
    // Treating them as opus matches what the Whisper-side decoder
    // (13.C) will see.
    if (lower.includes("webm") || lower.includes("ogg")) return "opus";
    if (lower.includes("mp4") || lower.includes("aac")) return "aac";
    if (lower.includes("pcm")) return "pcm16";
    return "unknown";
}

function mintSessionId(): string {
    if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
        return crypto.randomUUID();
    }
    // Test/legacy fallback.
    return `session-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
