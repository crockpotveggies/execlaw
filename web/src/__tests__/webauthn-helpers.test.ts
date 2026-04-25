// Coverage for the small base64url + options-coercion helpers in
// `auth/webauthn.ts`. They run inside jsdom (the real WebAuthn API
// is absent there), so we test the conversion shape rather than an
// end-to-end ceremony.

import { describe, expect, it } from "vitest";
import {
    coerceCreationOptions,
    coerceRequestOptions,
    serializeCredential,
    __testing,
} from "../auth/webauthn";

const { b64UrlToBuffer, bufferToB64Url } = __testing;

describe("base64url helpers", () => {
    it("round-trips an arbitrary byte buffer", () => {
        const bytes = new Uint8Array([0x00, 0x10, 0xfa, 0xfe, 0x7d, 0xff]);
        const enc = bufferToB64Url(bytes);
        // No padding, URL-safe alphabet (no '+' or '/').
        expect(enc).not.toContain("=");
        expect(enc).not.toContain("+");
        expect(enc).not.toContain("/");
        const back = new Uint8Array(b64UrlToBuffer(enc));
        expect(Array.from(back)).toEqual(Array.from(bytes));
    });

    it("decodes the canonical RFC 4648 examples", () => {
        // "abc" -> base64url "YWJj" (no padding)
        const buf = b64UrlToBuffer("YWJj");
        expect(new TextDecoder().decode(new Uint8Array(buf))).toBe("abc");
    });
});

describe("coerceCreationOptions", () => {
    it("converts the server's CreationOptions JSON to ArrayBuffer fields", () => {
        const raw = {
            publicKey: {
                challenge: "YWJj", // "abc"
                user: {
                    id: "ZGVm", // "def"
                    name: "alice",
                    displayName: "Alice",
                },
                rp: { id: "localhost", name: "execlaw" },
                pubKeyCredParams: [{ alg: -7, type: "public-key" as const }],
                excludeCredentials: [
                    {
                        id: "Z2hp", // "ghi"
                        type: "public-key" as const,
                        transports: ["usb" as AuthenticatorTransport],
                    },
                ],
            },
        };
        const out = coerceCreationOptions(raw);
        expect(out.challenge).toBeInstanceOf(ArrayBuffer);
        expect((out.user.id as ArrayBuffer).byteLength).toBe(3);
        // Decoded "def" → bytes [100,101,102]
        expect(Array.from(new Uint8Array(out.user.id as ArrayBuffer))).toEqual([
            100, 101, 102,
        ]);
        expect(out.rp.id).toBe("localhost");
        expect(out.excludeCredentials).toHaveLength(1);
        expect(
            (out.excludeCredentials![0].id as ArrayBuffer).byteLength,
        ).toBe(3);
    });
});

describe("coerceRequestOptions", () => {
    it("converts allowCredentials ids", () => {
        const raw = {
            publicKey: {
                challenge: "Y2hh", // "cha"
                allowCredentials: [
                    {
                        id: "Y3JlZA", // "cred"
                        type: "public-key" as const,
                    },
                ],
                userVerification: "preferred" as UserVerificationRequirement,
            },
        };
        const out = coerceRequestOptions(raw);
        expect(out.challenge).toBeInstanceOf(ArrayBuffer);
        expect(out.userVerification).toBe("preferred");
        expect(out.allowCredentials).toHaveLength(1);
        expect(
            (out.allowCredentials![0].id as ArrayBuffer).byteLength,
        ).toBe(4);
    });
});

describe("serializeCredential", () => {
    it("serialises an attestation credential to the wire shape", () => {
        const cred = {
            id: "id-string",
            rawId: new Uint8Array([1, 2, 3, 4]).buffer,
            type: "public-key",
            response: {
                clientDataJSON: new Uint8Array([5, 6]).buffer,
                attestationObject: new Uint8Array([7, 8, 9]).buffer,
            },
            getClientExtensionResults: () => ({}),
        } as unknown as PublicKeyCredential;
        const out = serializeCredential(cred);
        expect(out.id).toBe("id-string");
        expect(typeof out.rawId).toBe("string");
        expect(out.type).toBe("public-key");
        // Buffers [5,6] → base64url "BQY"; [7,8,9] → "BwgJ".
        expect((out.response as Record<string, string>).clientDataJSON).toBe(
            "BQY",
        );
        expect(
            (out.response as Record<string, string>).attestationObject,
        ).toBe("BwgJ");
    });

    it("serialises an assertion credential including null userHandle", () => {
        const cred = {
            id: "assertion-id",
            rawId: new Uint8Array([10, 11]).buffer,
            type: "public-key",
            response: {
                clientDataJSON: new Uint8Array([1]).buffer,
                authenticatorData: new Uint8Array([2, 3]).buffer,
                signature: new Uint8Array([4, 5, 6]).buffer,
                userHandle: null,
            },
            getClientExtensionResults: () => ({}),
        } as unknown as PublicKeyCredential;
        const out = serializeCredential(cred);
        const resp = out.response as Record<string, unknown>;
        expect(resp.userHandle).toBe(null);
        expect(typeof resp.signature).toBe("string");
    });
});
