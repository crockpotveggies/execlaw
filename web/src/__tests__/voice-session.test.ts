// Tests for the SPA VoiceSession framing helper (Phase 13.A closure).

import { describe, expect, it } from "vitest";
import { codecFromMime, VoiceSession } from "../chat/VoiceSession";

function decodeFrame(buf: ArrayBuffer): {
    headerLen: number;
    headerJson: string;
    payload: Uint8Array;
} {
    const view = new DataView(buf);
    const headerLen = view.getUint32(0, false);
    const headerBytes = new Uint8Array(buf, 4, headerLen);
    const headerJson = new TextDecoder().decode(headerBytes);
    const payload = new Uint8Array(buf, 4 + headerLen);
    return { headerLen, headerJson, payload };
}

describe("VoiceSession", () => {
    it("frames a payload with [u32 BE header_len][header JSON][payload]", () => {
        const sess = new VoiceSession({
            codec: "opus",
            sampleRate: 48000,
            sessionIdOverride: "11111111-1111-4111-8111-111111111111",
        });
        const payload = new TextEncoder().encode("hello-payload").buffer;
        const framed = sess.framePayload(payload);
        const { headerLen, headerJson, payload: outPayload } = decodeFrame(
            framed,
        );
        expect(headerLen).toBeGreaterThan(0);
        const header = JSON.parse(headerJson);
        expect(header.session).toBe("11111111-1111-4111-8111-111111111111");
        expect(header.seq).toBe(0);
        expect(header.codec).toBe("opus");
        expect(header.sample_rate).toBe(48000);
        expect(header.channels).toBe(1);
        expect(typeof header.ts_ms).toBe("number");
        expect(new TextDecoder().decode(outPayload)).toBe("hello-payload");
    });

    it("increments seq monotonically across frames", () => {
        const sess = new VoiceSession({ codec: "opus", sampleRate: 48000 });
        for (let i = 0; i < 5; i++) {
            const framed = sess.framePayload(new ArrayBuffer(0));
            const { headerJson } = decodeFrame(framed);
            expect(JSON.parse(headerJson).seq).toBe(i);
        }
        expect(sess.framesSent()).toBe(5);
    });

    it("preserves session id across frames", () => {
        const sess = new VoiceSession({ codec: "opus", sampleRate: 48000 });
        const ids = [0, 1, 2].map((_) => {
            const { headerJson } = decodeFrame(
                sess.framePayload(new ArrayBuffer(1)),
            );
            return JSON.parse(headerJson).session;
        });
        expect(new Set(ids).size).toBe(1);
    });

    it("handles empty payloads", () => {
        const sess = new VoiceSession({ codec: "opus", sampleRate: 48000 });
        const framed = sess.framePayload(new ArrayBuffer(0));
        const { payload } = decodeFrame(framed);
        expect(payload.byteLength).toBe(0);
    });
});

describe("codecFromMime", () => {
    it("maps webm/opus to opus", () => {
        expect(codecFromMime("audio/webm;codecs=opus")).toBe("opus");
        expect(codecFromMime("audio/webm")).toBe("opus");
    });

    it("maps mp4/aac to aac", () => {
        expect(codecFromMime("audio/mp4")).toBe("aac");
        expect(codecFromMime("audio/aac")).toBe("aac");
    });

    it("returns 'unknown' for unrecognized types", () => {
        expect(codecFromMime(undefined)).toBe("unknown");
        expect(codecFromMime("")).toBe("unknown");
        expect(codecFromMime("audio/something-else")).toBe("unknown");
    });

    it("is case-insensitive", () => {
        expect(codecFromMime("AUDIO/WEBM;CODECS=OPUS")).toBe("opus");
    });
});
