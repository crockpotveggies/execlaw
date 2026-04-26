// Phase 13.C — VoicePlayback decodes incoming PCM16 LE @ 24 kHz from
// the server's TTS pipeline and plays it through an AudioContext.
//
// The server emits `VoiceAudioOutbound { audio_b64, codec, seq }`
// events. Each chunk is base64'd PCM16 little-endian mono. The
// playback queue:
//
//   1. base64 → Uint8Array → Int16Array → Float32Array
//   2. wraps the Float32Array in an AudioBuffer at the source rate
//   3. schedules the buffer at `nextPlayStartTime` so consecutive
//      chunks join seamlessly (no inter-chunk click)
//   4. `flush()` cancels every pending source and rewinds the cursor
//      so a barge-in event halts playback within the next ~10ms
//
// The class is framework-agnostic so React components import it
// via plain hooks. AudioContext is created lazily — Safari + Chrome
// require a user gesture for the first construction, so we delay
// until the first `enqueue()` call.

export interface VoiceAudioOutbound {
    session: string;
    seq: number;
    codec: string;
    audio_b64: string;
}

export class VoicePlayback {
    private ctx: AudioContext | null = null;
    /// Sources currently scheduled. We keep references so `flush()`
    /// can stop them — Web Audio doesn't expose a "stop everything"
    /// primitive, so we track them ourselves.
    private liveSources: AudioBufferSourceNode[] = [];
    /// AudioContext time at which the next chunk should start. Lets
    /// consecutive chunks chain end-to-end without gaps or overlap.
    private nextPlayStartTime = 0;
    /// Sample rate of inbound audio. Matches Kokoro's native rate.
    private inboundSampleRate: number;

    constructor(inboundSampleRate: number = 24_000) {
        this.inboundSampleRate = inboundSampleRate;
    }

    /// Enqueue one VoiceAudioOutbound event for playback. Lazily
    /// constructs the AudioContext on first call. Silent no-op when
    /// the codec isn't recognized so future codec additions don't
    /// crash older clients.
    enqueue(ev: VoiceAudioOutbound): void {
        if (ev.codec !== "pcm16le" && ev.codec !== "pcm16") {
            return;
        }
        const samples = decodeBase64Pcm16(ev.audio_b64);
        if (samples.length === 0) return;
        const ctx = this.ensureContext();
        const buffer = ctx.createBuffer(
            1,
            samples.length,
            this.inboundSampleRate,
        );
        const channel = buffer.getChannelData(0);
        // Convert i16 → f32 in [-1, 1].
        for (let i = 0; i < samples.length; i++) {
            channel[i] = samples[i] / 32768.0;
        }
        const src = ctx.createBufferSource();
        src.buffer = buffer;
        src.connect(ctx.destination);
        const now = ctx.currentTime;
        const startAt = Math.max(now, this.nextPlayStartTime);
        src.start(startAt);
        this.nextPlayStartTime =
            startAt + samples.length / this.inboundSampleRate;
        this.liveSources.push(src);
        src.onended = () => {
            // Drop ourselves from the live list — small GC nudge.
            const idx = this.liveSources.indexOf(src);
            if (idx >= 0) this.liveSources.splice(idx, 1);
        };
    }

    /// Halt playback immediately. Called on `VoiceInterrupted`.
    /// Safe to call before any audio has played; safe to call
    /// repeatedly.
    flush(): void {
        for (const src of this.liveSources) {
            try {
                src.stop();
            } catch {
                /* already stopped */
            }
            src.disconnect();
        }
        this.liveSources = [];
        if (this.ctx) {
            this.nextPlayStartTime = this.ctx.currentTime;
        }
    }

    /// Closed permanently. Ditches the AudioContext so the browser
    /// can release its audio thread. Useful when the chat tab is
    /// being unmounted; safe to recreate via `enqueue` afterward.
    close(): void {
        this.flush();
        if (this.ctx) {
            void this.ctx.close().catch(() => {
                /* ignore */
            });
            this.ctx = null;
        }
        this.nextPlayStartTime = 0;
    }

    /// Test-only inspection.
    get pendingSourceCount(): number {
        return this.liveSources.length;
    }

    private ensureContext(): AudioContext {
        if (this.ctx) return this.ctx;
        // The "any" cast accommodates webkit-prefixed AudioContext on
        // older Safari. We don't import a polyfill — the prefix has
        // been gone since Safari 14 (2020) and execlaw's locked
        // browser baseline is Chromium 120+ / Firefox 120+ / Safari
        // 17+ anyway.
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const Ctor = (window as any).AudioContext ?? (window as any).webkitAudioContext;
        if (!Ctor) {
            throw new Error("AudioContext unavailable in this browser");
        }
        const ctx = new Ctor() as AudioContext;
        this.ctx = ctx;
        this.nextPlayStartTime = ctx.currentTime;
        return ctx;
    }
}

/// base64 → Int16Array of PCM16 LE samples. Exported for test cover.
export function decodeBase64Pcm16(b64: string): Int16Array {
    const binary = atob(b64);
    const len = binary.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) {
        bytes[i] = binary.charCodeAt(i);
    }
    // Even-length only; drop the dangling odd byte if a misbehaving
    // server emits one (matches the server-side parser's contract).
    const sampleCount = bytes.length >> 1;
    const out = new Int16Array(sampleCount);
    for (let i = 0; i < sampleCount; i++) {
        const lo = bytes[i * 2];
        const hi = bytes[i * 2 + 1];
        // Two's-complement reconstruction.
        let v = lo | (hi << 8);
        if (v >= 0x8000) v -= 0x10000;
        out[i] = v;
    }
    return out;
}
