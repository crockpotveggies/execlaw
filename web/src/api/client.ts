// Thin fetch wrapper around the Rust server's REST API.
//
// All requests go through `apiFetch` so:
// - the access token is attached automatically when we have one,
// - JSON parsing + structured-error mapping happens in one place,
// - an unauthenticated 401 surfaces as `ApiError("unauthorized")`
//   rather than a raw fetch failure (the AuthContext keys off this
//   to clear stale tokens).
//
// Same-origin in production (rust-embed serves the bundle); same-origin-
// like in dev because Vite proxies /api → :3030.

import {
    reportRestNetworkError,
    reportRestSuccess,
} from "./connection";

export type ApiErrorCode =
    | "network"
    | "unauthorized"
    | "conflict"
    | "bad_request"
    | "server"
    | "unknown";

export class ApiError extends Error {
    code: ApiErrorCode;
    status: number;
    serverCode: string | undefined;

    constructor(
        code: ApiErrorCode,
        message: string,
        status: number,
        serverCode?: string,
    ) {
        super(message);
        this.code = code;
        this.status = status;
        this.serverCode = serverCode;
    }
}

export interface ApiFetchOptions {
    method?: "GET" | "POST" | "PATCH" | "PUT" | "DELETE";
    body?: unknown;
    /** Override the bearer token (used by `/api/setup`-success → first /me probe). */
    accessToken?: string | null;
    /** Skip JSON parsing — caller wants the raw text body (used by /api/ping). */
    rawText?: boolean;
    /** Internal — set when this is the post-refresh retry, prevents infinite loops. */
    _isRetry?: boolean;
}

const DEFAULT_HEADERS: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json",
};

/**
 * Hook that performs a refresh-token rotation when an access token is
 * rejected with 401. Returns the new access token on success, or
 * `null` if refresh failed (token expired / revoked / network error).
 *
 * AuthContext installs this on boot via {@link setRefreshHook}; tests
 * can install their own to assert the auto-retry behaviour without
 * standing up a real auth context.
 *
 * Implementations MUST be idempotent under concurrent invocation —
 * `apiFetch` may receive multiple parallel 401s before the first
 * refresh resolves. The hook handles the dedup internally.
 */
export type RefreshHook = () => Promise<string | null>;

let installedRefreshHook: RefreshHook | null = null;

/** Install the refresh hook used by `apiFetch` for silent retries. */
export function setRefreshHook(hook: RefreshHook | null): void {
    installedRefreshHook = hook;
}

/** Test seam — peek at the currently-installed hook. */
export function _getRefreshHook(): RefreshHook | null {
    return installedRefreshHook;
}

/**
 * Make an authenticated REST call.
 *
 * Returns the parsed JSON body on 2xx (or the raw text when
 * `rawText: true`), throws `ApiError` on every other shape so callers
 * can branch on `err.code`.
 */
export async function apiFetch<T>(
    path: string,
    opts: ApiFetchOptions = {},
    tokenAccessor: () => string | null = () => null,
): Promise<T> {
    const headers: Record<string, string> = { ...DEFAULT_HEADERS };
    const token = opts.accessToken !== undefined ? opts.accessToken : tokenAccessor();
    if (token) {
        headers.authorization = `Bearer ${token}`;
    }
    if (opts.rawText) {
        delete headers["content-type"];
    }

    let resp: Response;
    try {
        resp = await fetch(path, {
            method: opts.method ?? "GET",
            headers,
            body:
                opts.body === undefined
                    ? undefined
                    : typeof opts.body === "string"
                      ? opts.body
                      : JSON.stringify(opts.body),
        });
    } catch (e) {
        // 2026-04-28 — connection-health surface. NetworkError is a
        // dev-server-restart / DNS / CORS hiccup; the operator wants
        // a "Reconnecting…" banner, not a logout. We report and
        // rethrow; AuthContext keys off code === "network" to keep
        // tokens warm.
        reportRestNetworkError();
        throw new ApiError(
            "network",
            `network error talking to ${path}: ${(e as Error).message ?? e}`,
            0,
        );
    }
    // Successful round-trip (any HTTP status) clears the offline
    // grace window — the server is reachable, even if the response
    // is a 4xx / 5xx.
    reportRestSuccess();

    if (opts.rawText) {
        const text = await resp.text();
        if (!resp.ok) {
            throw mapStatus(resp.status, text);
        }
        return text as unknown as T;
    }

    const text = await resp.text();
    let parsed: unknown = null;
    if (text.length > 0) {
        try {
            parsed = JSON.parse(text);
        } catch {
            // Server returned non-JSON on a JSON-expecting path — treat
            // as a malformed server response.
            throw new ApiError(
                "server",
                `non-JSON response from ${path}: ${text.slice(0, 120)}`,
                resp.status,
            );
        }
    }
    if (!resp.ok) {
        const serverCode = readServerCode(parsed);
        const message = readServerMessage(parsed) ?? resp.statusText;
        // Phase 7 hardening: auto-retry on 401 once. AuthContext
        // installs a `RefreshHook` that calls /api/token/refresh and
        // updates the in-memory access-token ref. Skip the retry
        // when:
        //   - this call was already a retry (_isRetry guards the
        //     base case so a double-401 surfaces normally),
        //   - no hook is installed (e.g. boot-time before
        //     AuthContext mounts, or unauth routes like /api/login),
        //   - the caller explicitly passed `accessToken` — they
        //     want to control the credential, not have it rotated
        //     under them,
        //   - the request IS the refresh call itself. Letting the
        //     hook re-enter on a /api/token/refresh failure causes
        //     a deadlock: performRefresh's in-flight promise is
        //     awaiting THIS apiFetch, and silent-retry would await
        //     the same in-flight promise → both pending forever.
        //     This is the root cause of Firefox tabs OOM-ing during
        //     a backend restart when the persisted refresh token
        //     happens to be invalid.
        if (
            resp.status === 401 &&
            !opts._isRetry &&
            opts.accessToken === undefined &&
            installedRefreshHook !== null &&
            !isAuthRefreshPath(path)
        ) {
            const fresh = await installedRefreshHook();
            if (fresh) {
                return apiFetch<T>(
                    path,
                    { ...opts, _isRetry: true },
                    () => fresh,
                );
            }
        }
        throw mapStatus(resp.status, message, serverCode);
    }
    return parsed as T;
}

/// Auth endpoints whose 401 must NOT trigger silent-retry. Any call
/// that the refresh hook itself depends on belongs here.
function isAuthRefreshPath(path: string): boolean {
    return (
        path === "/api/token/refresh" ||
        path === "/api/login" ||
        path === "/api/login/webauthn/finish" ||
        path === "/api/setup"
    );
}

function mapStatus(
    status: number,
    message: string,
    serverCode?: string,
): ApiError {
    if (status === 401) return new ApiError("unauthorized", message, status, serverCode);
    if (status === 409) return new ApiError("conflict", message, status, serverCode);
    if (status >= 400 && status < 500)
        return new ApiError("bad_request", message, status, serverCode);
    if (status >= 500) return new ApiError("server", message, status, serverCode);
    return new ApiError("unknown", message, status, serverCode);
}

function readServerCode(parsed: unknown): string | undefined {
    if (
        typeof parsed === "object" &&
        parsed !== null &&
        "error" in parsed &&
        typeof (parsed as { error?: unknown }).error === "object" &&
        (parsed as { error?: { code?: unknown } }).error !== null
    ) {
        const code = (parsed as { error: { code?: unknown } }).error.code;
        return typeof code === "string" ? code : undefined;
    }
    return undefined;
}

function readServerMessage(parsed: unknown): string | undefined {
    if (
        typeof parsed === "object" &&
        parsed !== null &&
        "error" in parsed &&
        typeof (parsed as { error?: unknown }).error === "object" &&
        (parsed as { error?: { message?: unknown } }).error !== null
    ) {
        const msg = (parsed as { error: { message?: unknown } }).error.message;
        return typeof msg === "string" ? msg : undefined;
    }
    return undefined;
}
