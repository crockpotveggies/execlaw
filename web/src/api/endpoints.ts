// Strongly-typed wrappers around the execlaw REST surface used by the
// SPA. Keep these schemas mirrored with the Rust `routes.rs` /
// `chats.rs` / `plugins.rs` payload shapes — there is no contract
// generator yet (Phase 7+ adds OpenAPI codegen).

import { ApiError, apiFetch } from "./client";

// ---- /api/ping -----------------------------------------------------

export type PingState = "setup" | "pong";

/**
 * Probe the server for its setup state. Returns "setup" when no
 * controller user exists yet, "pong" once setup has completed.
 *
 * Any non-2xx response (including network failures) is surfaced as a
 * thrown ApiError so the caller can show a "can't reach server"
 * banner instead of routing to a wrong screen.
 */
export async function ping(): Promise<PingState> {
    const text = await apiFetch<string>("/api/ping", { rawText: true });
    if (text === "setup") return "setup";
    if (text === "pong") return "pong";
    // Defensive: a future server might add states; treat anything
    // unrecognized as "needs setup" rather than crashing the SPA.
    throw new ApiError(
        "unknown",
        `unexpected /api/ping response: ${text}`,
        200,
    );
}

// ---- /api/setup ----------------------------------------------------

export interface SetupRequest {
    /** Login handle. Server-side normalized to lowercase; 3-32 chars, [a-z0-9_-]. */
    username: string;
    admin_password: string;
    display_name: string;
    email?: string;
}

export interface SetupResponse {
    principal_id: string;
    access_token: string;
    refresh_token: string;
}

export async function postSetup(req: SetupRequest): Promise<SetupResponse> {
    return apiFetch<SetupResponse>("/api/setup", {
        method: "POST",
        body: req,
    });
}

// ---- /api/login ----------------------------------------------------

export interface LoginRequest {
    username: string;
    admin_password: string;
}

export interface LoginResponse {
    access_token: string;
    refresh_token: string;
}

export async function postLogin(req: LoginRequest): Promise<LoginResponse> {
    return apiFetch<LoginResponse>("/api/login", {
        method: "POST",
        body: req,
    });
}

// ---- /api/chats ----------------------------------------------------

export interface ThreadSummary {
    conversation_id: string;
    kind: string;
    phase: string;
    trust_class: string;
    modality: string;
    display_name: string | null;
    is_pinned: boolean;
    is_ephemeral: boolean;
    ephemeral_expires_at: number | null;
    last_seq: number;
}

export interface ThreadListResponse {
    threads: ThreadSummary[];
}

export async function listThreads(
    tokenAccessor: () => string | null,
): Promise<ThreadListResponse> {
    return apiFetch<ThreadListResponse>("/api/chats", {}, tokenAccessor);
}

// ---- /api/chats/:id/messages ---------------------------------------

export interface MessageView {
    seq: number;
    kind: string;
    text: string | null;
    actor: string | null;
    committed_at: number;
}

export interface MessagesListResponse {
    conversation_id: string;
    messages: MessageView[];
}

export async function listMessages(
    conversationId: string,
    tokenAccessor: () => string | null,
    opts: { before?: number; limit?: number } = {},
): Promise<MessagesListResponse> {
    const qs = new URLSearchParams();
    if (opts.before !== undefined) qs.set("before", String(opts.before));
    if (opts.limit !== undefined) qs.set("limit", String(opts.limit));
    const path = `/api/chats/${encodeURIComponent(conversationId)}/messages${
        qs.toString() ? "?" + qs.toString() : ""
    }`;
    return apiFetch<MessagesListResponse>(path, {}, tokenAccessor);
}

export interface SendMessageRequest {
    text: string;
    sender_principal_id?: string;
}

export interface SendMessageResponse {
    user_msg_seq?: number;
    assistant_text?: string;
    assistant_msg_seq?: number;
    [extra: string]: unknown;
}

export async function postMessage(
    conversationId: string,
    body: SendMessageRequest,
    tokenAccessor: () => string | null,
): Promise<SendMessageResponse> {
    return apiFetch<SendMessageResponse>(
        `/api/chats/${encodeURIComponent(conversationId)}/messages`,
        { method: "POST", body },
        tokenAccessor,
    );
}

// ---- /api/admin/me -------------------------------------------------

export interface MeResponse {
    user_id: string;
    username: string;
    display_name: string;
    email: string | null;
    role: string;
    last_login_at: number | null;
}

export async function getMe(
    tokenAccessor: () => string | null,
): Promise<MeResponse> {
    return apiFetch<MeResponse>(
        "/api/admin/me",
        {},
        tokenAccessor,
    );
}

// ---- /api/token/refresh -------------------------------------------

export interface RefreshResponse {
    access_token: string;
    refresh_token: string;
}

export async function postRefresh(refreshToken: string): Promise<RefreshResponse> {
    return apiFetch<RefreshResponse>("/api/token/refresh", {
        method: "POST",
        body: { refresh_token: refreshToken },
    });
}

// ---- /api/logout ---------------------------------------------------

export async function postLogout(refreshToken: string | null): Promise<void> {
    await apiFetch<{ ok: boolean }>("/api/logout", {
        method: "POST",
        body: refreshToken ? { refresh_token: refreshToken } : {},
    });
}
