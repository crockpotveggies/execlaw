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
}

const DEFAULT_HEADERS: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json",
};

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
        throw new ApiError(
            "network",
            `network error talking to ${path}: ${(e as Error).message ?? e}`,
            0,
        );
    }

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
        throw mapStatus(resp.status, message, serverCode);
    }
    return parsed as T;
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
