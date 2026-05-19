// Signed download URL helper.
//
// Replaces the pre-2026-05-19 `?access_token=<jwt>` pattern that the
// SPA appended to attachment URLs so `<a download>` / `<img src>`
// could authenticate. Security audit flagged that pattern: a full-
// access JWT travels through browser history, proxy logs, referrer
// headers, and copied-link shares — and a JWT authorises every API
// call.
//
// New flow: the SPA POSTs `{ path }` to `/api/downloads/sign` with
// its Bearer header; the server returns a URL carrying only
// `?exp&user&sig`. The `sig` is an HMAC bound to the path AND user
// AND expiry, so a leaked signed URL grants no authority beyond
// "GET this exact path as this user before exp."
//
// Default TTL is 5 minutes server-side. Components cache the
// resolved URL until they unmount or the path changes.

import { apiFetch } from "./client";

export interface SignDownloadUrlResponse {
    url: string;
    expires_at: number;
}

/**
 * Sign a path for browser-direct GET. The returned URL is a single
 * same-origin string suitable for `<a download href>` /
 * `<img src>` / `<video src>` / `window.open`. Authority is bound
 * to the caller's user_id at sign time; the URL cannot be used by
 * a different user.
 *
 * Path must be on the server-side allowlist (currently:
 * `/api/attachments/...`). Other paths return a 403, which the
 * caller surfaces as a rejected Promise.
 */
export async function signDownloadUrl(
    path: string,
    tokenAccessor: () => string | null,
    ttlSecs?: number,
): Promise<string> {
    const resp = await apiFetch<SignDownloadUrlResponse>(
        "/api/downloads/sign",
        {
            method: "POST",
            body: ttlSecs !== undefined ? { path, ttl_secs: ttlSecs } : { path },
        },
        tokenAccessor,
    );
    return resp.url;
}
