// Browser-side helpers for the Phase 7e WebAuthn ceremony.
//
// `webauthn-rs` serialises `PublicKeyCredentialCreationOptions` and
// `PublicKeyCredentialRequestOptions` as JSON with base64url strings
// in the binary slots (challenge, user.id, allowCredentials[].id,
// excludeCredentials[].id). The browser's `navigator.credentials`
// API wants ArrayBuffers in those slots. These helpers do the
// conversion both ways without a heavy SDK dep.
//
// Restricting ourselves to what the relying party actually emits
// keeps the helper small enough to unit-test in jsdom (where the
// real WebAuthn API is absent).

/// Decode a base64url string (no padding) into an ArrayBuffer.
function b64UrlToBuffer(s: string): ArrayBuffer {
    const pad = "=".repeat((4 - (s.length % 4)) % 4);
    const b64 = (s + pad).replace(/-/g, "+").replace(/_/g, "/");
    const raw = atob(b64);
    const buf = new ArrayBuffer(raw.length);
    const view = new Uint8Array(buf);
    for (let i = 0; i < raw.length; i++) view[i] = raw.charCodeAt(i);
    return buf;
}

/// Encode an ArrayBuffer (or view) as a base64url string (no padding).
function bufferToB64Url(buf: ArrayBuffer | ArrayBufferView): string {
    const bytes =
        buf instanceof ArrayBuffer
            ? new Uint8Array(buf)
            : new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    let s = "";
    for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

interface ServerCreationOptions {
    publicKey: {
        challenge: string;
        user: { id: string; name: string; displayName: string };
        rp: { id?: string; name: string };
        pubKeyCredParams: Array<{ alg: number; type: "public-key" }>;
        timeout?: number;
        attestation?: AttestationConveyancePreference;
        excludeCredentials?: Array<{
            id: string;
            type: "public-key";
            transports?: AuthenticatorTransport[];
        }>;
        authenticatorSelection?: AuthenticatorSelectionCriteria;
        extensions?: Record<string, unknown>;
    };
}

interface ServerRequestOptions {
    publicKey: {
        challenge: string;
        timeout?: number;
        rpId?: string;
        userVerification?: UserVerificationRequirement;
        allowCredentials?: Array<{
            id: string;
            type: "public-key";
            transports?: AuthenticatorTransport[];
        }>;
        extensions?: Record<string, unknown>;
    };
}

/// Convert the server's CreationOptions JSON into something the
/// browser's `navigator.credentials.create()` accepts.
export function coerceCreationOptions(
    raw: unknown,
): PublicKeyCredentialCreationOptions {
    const opts = raw as ServerCreationOptions;
    const pk = opts.publicKey;
    return {
        challenge: b64UrlToBuffer(pk.challenge),
        user: {
            id: b64UrlToBuffer(pk.user.id),
            name: pk.user.name,
            displayName: pk.user.displayName,
        },
        rp: { id: pk.rp.id, name: pk.rp.name },
        pubKeyCredParams: pk.pubKeyCredParams,
        timeout: pk.timeout,
        attestation: pk.attestation,
        excludeCredentials: pk.excludeCredentials?.map((c) => ({
            id: b64UrlToBuffer(c.id),
            type: c.type,
            transports: c.transports,
        })),
        authenticatorSelection: pk.authenticatorSelection,
        extensions: pk.extensions,
    } as PublicKeyCredentialCreationOptions;
}

/// Convert the server's RequestOptions JSON into something the
/// browser's `navigator.credentials.get()` accepts.
export function coerceRequestOptions(
    raw: unknown,
): PublicKeyCredentialRequestOptions {
    const opts = raw as ServerRequestOptions;
    const pk = opts.publicKey;
    return {
        challenge: b64UrlToBuffer(pk.challenge),
        timeout: pk.timeout,
        rpId: pk.rpId,
        userVerification: pk.userVerification,
        allowCredentials: pk.allowCredentials?.map((c) => ({
            id: b64UrlToBuffer(c.id),
            type: c.type,
            transports: c.transports,
        })),
        extensions: pk.extensions,
    } as PublicKeyCredentialRequestOptions;
}

/// Serialise a `PublicKeyCredential` (output of create/get) into the
/// JSON shape `webauthn-rs::finish_passkey_*` expects on the wire.
///
/// Uses duck-typing on the response shape rather than `instanceof`
/// — `AuthenticatorAttestationResponse` and friends aren't available
/// in jsdom, so the unit tests would otherwise need their own mocks
/// in environments where the constructors don't exist.
export function serializeCredential(
    cred: PublicKeyCredential,
): Record<string, unknown> {
    const resp = cred.response as
        | AuthenticatorAttestationResponse
        | AuthenticatorAssertionResponse;
    const ext = (cred.getClientExtensionResults?.() ?? {}) as Record<
        string,
        unknown
    >;
    const base = {
        id: cred.id,
        rawId: bufferToB64Url(cred.rawId),
        type: cred.type,
        extensions: ext,
    };
    if ("attestationObject" in resp) {
        const a = resp as AuthenticatorAttestationResponse;
        return {
            ...base,
            response: {
                clientDataJSON: bufferToB64Url(a.clientDataJSON),
                attestationObject: bufferToB64Url(a.attestationObject),
            },
        };
    }
    if ("signature" in resp) {
        const a = resp as AuthenticatorAssertionResponse;
        return {
            ...base,
            response: {
                clientDataJSON: bufferToB64Url(a.clientDataJSON),
                authenticatorData: bufferToB64Url(a.authenticatorData),
                signature: bufferToB64Url(a.signature),
                userHandle: a.userHandle ? bufferToB64Url(a.userHandle) : null,
            },
        };
    }
    throw new Error("unknown PublicKeyCredential response shape");
}

// Internal helpers exported for unit tests only.
export const __testing = { b64UrlToBuffer, bufferToB64Url };
