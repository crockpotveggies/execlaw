// Tests for VoicePlayback (Phase 13.C).
//
// AudioContext is a browser API jsdom doesn't ship; we cover the
// decoder + the lazy-construct guard here, and rely on the e2e
// preview tests for the actual playback round-trip.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
    VoicePlayback,
    decodeBase64Pcm16,
} from "../chat/VoicePlayback";

function pcmToBase64(samples: Int16Array): string {
    const bytes = new Uint8Array(samples.length * 2);
    for (let i = 0; i < samples.length; i++) {
        const v = samples[i] < 0 ? samples[i] + 0x10000 : samples[i];
        bytes[i * 2] = v & 0xff;
        bytes[i * 2 + 1] = (v >> 8) & 0xff;
    }
    let binary = "";
    for (let i = 0; i < bytes.length; i++) {
        binary += String.fromCharCode(bytes[i]);
    }
    return btoa(binary);
}

describe("decodeBase64Pcm16", () => {
    it("round-trips signed PCM16 samples", () => {
        const samples = Int16Array.of(0, 1, -1, 32_767, -32_768, 12_345);
        const b64 = pcmToBase64(samples);
        const got = decodeBase64Pcm16(b64);
        expect(Array.from(got)).toEqual(Array.from(samples));
    });

    it("returns an empty Int16Array for empty input", () => {
        expect(decodeBase64Pcm16("").length).toBe(0);
    });

    it("drops a dangling odd byte rather than throwing", () => {
        // 5 bytes = 2 valid samples + 1 dangling — must NOT throw.
        const bytes = new Uint8Array([0x01, 0x00, 0xfe, 0xff, 0x42]);
        let binary = "";
        for (let i = 0; i < bytes.length; i++) {
            binary += String.fromCharCode(bytes[i]);
        }
        const b64 = btoa(binary);
        const got = decodeBase64Pcm16(b64);
        expect(got.length).toBe(2);
        expect(got[0]).toBe(1);
        expect(got[1]).toBe(-2);
    });
});

describe("VoicePlayback", () => {
    let mockContext: {
        currentTime: number;
        destination: object;
        createBuffer: ReturnType<typeof vi.fn>;
        createBufferSource: ReturnType<typeof vi.fn>;
        close: ReturnType<typeof vi.fn>;
    };
    let liveSources: Array<{
        start: ReturnType<typeof vi.fn>;
        stop: ReturnType<typeof vi.fn>;
        disconnect: ReturnType<typeof vi.fn>;
        connect: ReturnType<typeof vi.fn>;
        onended: (() => void) | null;
        buffer: object | null;
    }>;
    let originalAudioContext: unknown;

    beforeEach(() => {
        liveSources = [];
        mockContext = {
            currentTime: 0,
            destination: {},
            createBuffer: vi.fn(
                (
                    _channels: number,
                    sampleCount: number,
                    _rate: number,
                ) => ({
                    getChannelData: () => new Float32Array(sampleCount),
                }),
            ),
            createBufferSource: vi.fn(() => {
                const src = {
                    start: vi.fn(),
                    stop: vi.fn(),
                    disconnect: vi.fn(),
                    connect: vi.fn(),
                    onended: null as (() => void) | null,
                    buffer: null as object | null,
                };
                liveSources.push(src);
                return src;
            }),
            close: vi.fn(() => Promise.resolve()),
        };
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        originalAudioContext = (window as any).AudioContext;
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (window as any).AudioContext = vi.fn(() => mockContext);
    });

    afterEach(() => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (window as any).AudioContext = originalAudioContext;
    });

    it("does not construct an AudioContext until the first enqueue", () => {
        new VoicePlayback();
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const ctor = (window as any).AudioContext as ReturnType<typeof vi.fn>;
        expect(ctor).not.toHaveBeenCalled();
    });

    it("ignores enqueues with an unknown codec", () => {
        const pb = new VoicePlayback();
        pb.enqueue({
            session: "s1",
            seq: 1,
            codec: "opus",
            audio_b64: pcmToBase64(Int16Array.of(1, 2, 3)),
        });
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const ctor = (window as any).AudioContext as ReturnType<typeof vi.fn>;
        expect(ctor).not.toHaveBeenCalled();
    });

    it("schedules a buffer source per enqueued chunk", () => {
        const pb = new VoicePlayback();
        pb.enqueue({
            session: "s1",
            seq: 1,
            codec: "pcm16le",
            audio_b64: pcmToBase64(Int16Array.of(1, 2, 3, 4)),
        });
        pb.enqueue({
            session: "s1",
            seq: 2,
            codec: "pcm16le",
            audio_b64: pcmToBase64(Int16Array.of(5, 6, 7, 8)),
        });
        expect(liveSources.length).toBe(2);
        expect(liveSources[0].start).toHaveBeenCalledOnce();
        expect(liveSources[1].start).toHaveBeenCalledOnce();
        expect(pb.pendingSourceCount).toBe(2);
    });

    it("flush stops every live source and resets the cursor", () => {
        const pb = new VoicePlayback();
        pb.enqueue({
            session: "s1",
            seq: 1,
            codec: "pcm16le",
            audio_b64: pcmToBase64(Int16Array.of(1, 2)),
        });
        pb.enqueue({
            session: "s1",
            seq: 2,
            codec: "pcm16le",
            audio_b64: pcmToBase64(Int16Array.of(3, 4)),
        });
        pb.flush();
        expect(liveSources[0].stop).toHaveBeenCalled();
        expect(liveSources[1].stop).toHaveBeenCalled();
        expect(pb.pendingSourceCount).toBe(0);
    });

    it("close() releases the AudioContext", () => {
        const pb = new VoicePlayback();
        pb.enqueue({
            session: "s1",
            seq: 1,
            codec: "pcm16le",
            audio_b64: pcmToBase64(Int16Array.of(1)),
        });
        pb.close();
        expect(mockContext.close).toHaveBeenCalled();
        expect(pb.pendingSourceCount).toBe(0);
    });

    it("ignores empty chunks without scheduling a source", () => {
        const pb = new VoicePlayback();
        pb.enqueue({
            session: "s1",
            seq: 1,
            codec: "pcm16le",
            audio_b64: "",
        });
        expect(liveSources.length).toBe(0);
    });
});
