// Strongly-typed wrappers around the execlaw REST surface used by the
// SPA. Keep these schemas mirrored with the Rust `routes.rs` /
// `chats.rs` / `plugins.rs` payload shapes — there is no contract
// generator yet (Phase 7+ adds OpenAPI codegen).

import { ApiError, apiFetch } from "./client";

// ---- /api/ping -----------------------------------------------------

export type PingState = "setup" | "wizard" | "pong";

/**
 * Probe the server for its setup state. Three-state response:
 *
 *   * "setup"  — no controller user yet (account-creation flow).
 *   * "wizard" — controller exists but the first-run wizard hasn't
 *                completed (no Standard backend configured AND
 *                operator hasn't dismissed). SPA routes back to
 *                /setup, resuming at the docker step.
 *   * "pong"   — fully provisioned (or wizard dismissed).
 *
 * Any non-2xx response (including network failures) is surfaced as a
 * thrown ApiError so the caller can show a "can't reach server"
 * banner instead of routing to a wrong screen.
 */
export async function ping(): Promise<PingState> {
    const text = await apiFetch<string>("/api/ping", { rawText: true });
    if (text === "setup") return "setup";
    if (text === "wizard") return "wizard";
    if (text === "pong") return "pong";
    // Defensive: a future server might add states; treat anything
    // unrecognized as "needs setup" rather than crashing the SPA.
    throw new ApiError(
        "unknown",
        `unexpected /api/ping response: ${text}`,
        200,
    );
}

/**
 * Phase 14 — explicit "Skip for now" on the first-run wizard's
 * backend step. POSTs to mark the wizard dismissed; subsequent
 * `ping()` calls return "pong" so the SPA stops bouncing the
 * operator back to /setup.
 */
export async function dismissSetupWizard(
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        "/api/admin/setup/dismiss",
        { method: "POST", body: {} },
        tokenAccessor,
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

// ---- /api/logout/all (Phase 7 hardening) ---------------------------

export interface LogoutAllResponse {
    revoked_session_count: number;
}

/// "Sign out everywhere" — revokes every refresh token bound to the
/// caller's user_id on the server. The caller is identified from
/// the Bearer token, never from the request body, so a stolen
/// refresh token alone can't trigger this for someone else.
export async function postLogoutAll(
    tokenAccessor: () => string | null,
): Promise<LogoutAllResponse> {
    return apiFetch<LogoutAllResponse>(
        "/api/logout/all",
        { method: "POST", body: {} },
        tokenAccessor,
    );
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

/// Phase 7e: superset of `postLogin` that returns the new
/// `LoginOutcome` shape (defined further down in this file). The
/// server returns either the legacy `{access_token, refresh_token}`
/// pair OR a webauthn challenge — callers must branch on
/// `webauthn_required`. The non-discriminated `unknown` here is
/// intentional; the call site narrows after the flag check.
export async function postLoginOutcome(
    req: LoginRequest,
): Promise<unknown> {
    return apiFetch<unknown>("/api/login", {
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
    /// Wall-clock unix-seconds of the last committed turn. The
    /// server orders the list by this so most-recent-activity
    /// floats to the top of the sidebar (under any pinned rows).
    /// Optional only because tests + older fixtures don't supply it;
    /// new code paths always send it.
    last_activity_at?: number;
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
    /**
     * Originating transport when this message flowed through a
     * bridge (signal / email / voice / sms). Absent for the default
     * web path. The chat view renders a per-message channel icon
     * when set so the operator can tell at a glance "this came in
     * via Signal" / "the agent replied via Signal".
     */
    channel_origin?: "signal" | "email" | "voice" | "sms" | null;
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

/// Card-history projection for a conversation. The chat-pane
/// fetches this on thread load (alongside `listMessages`) and
/// seeds `cardStore` so a page refresh re-hydrates inline cards
/// (research card, attachment chip, etc.). Without this fetch,
/// the card store starts empty on refresh and chips vanish even
/// though the underlying CardOpened/Closed events are durably
/// persisted server-side.
///
/// Card shape mirrors `core::cards::Card` — see web/src/cards/types.ts.
export interface ListCardsResponse {
    conversation_id: string;
    /// Ordered oldest → newest (by opened_at on the server side).
    /// Use as-is; card_id is the de-dupe key in the store.
    cards: Array<{
        card_id: string;
        conversation_id: string;
        kind: string;
        state: string;
        title: string;
        summary: string;
        progress: number | null;
        phase: string | null;
        details: unknown;
        actions: ReadonlyArray<unknown>;
        error: string | null;
        opened_at: number;
        updated_at: number;
        attachment_id?: string | null;
        event_seq?: number | null;
    }>;
}

export async function listCards(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<ListCardsResponse> {
    return apiFetch<ListCardsResponse>(
        `/api/chats/${encodeURIComponent(conversationId)}/cards`,
        {},
        tokenAccessor,
    );
}

export interface SendMessageRequest {
    text: string;
    sender_principal_id?: string;
    /// 2026-04-28 — when true, server runs the turn but skips
    /// every persistent write. Streaming token deltas + phase
    /// events still broadcast on the WS bus keyed on
    /// `conversation_id` so the SPA renders identically.
    incognito?: boolean;
    /// Running transcript for incognito turns. Required on every
    /// incognito send because the server can't replay the event
    /// log. Ignored when `incognito` is false/missing.
    prior_messages?: PriorMessage[];
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

// ---- /api/chats/:id/stop -------------------------------------------

/// Halt the in-flight turn for a conversation (chat stop button).
/// Idempotent on the server — `cancelled` is `false` when no turn was
/// in flight, but the SPA can fire-and-forget either way.
export interface StopTurnResponse {
    conversation_id: string;
    cancelled: boolean;
}

export async function postStopTurn(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<StopTurnResponse> {
    return apiFetch<StopTurnResponse>(
        `/api/chats/${encodeURIComponent(conversationId)}/stop`,
        { method: "POST" },
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

// ---- Thread metadata write (PATCH /api/chats/:id) ------------------

export interface PatchThreadRequest {
    /** Set or null-clear the display name. Omit to leave alone. */
    display_name?: string | null;
    is_pinned?: boolean;
    is_ephemeral?: boolean;
    ephemeral_expires_at?: number;
}

export interface PatchThreadResponse {
    conversation_id: string;
    display_name: string | null;
    is_pinned: boolean;
    is_ephemeral: boolean;
    ephemeral_expires_at: number | null;
}

export async function patchThread(
    conversationId: string,
    req: PatchThreadRequest,
    tokenAccessor: () => string | null,
): Promise<PatchThreadResponse> {
    return apiFetch<PatchThreadResponse>(
        `/api/chats/${encodeURIComponent(conversationId)}`,
        { method: "PATCH", body: req },
        tokenAccessor,
    );
}

/// 2026-04-28 — incognito turn message envelope. Sent as
/// `prior_messages` on `SendMessageRequest` when `incognito = true`.
/// Server reads this in place of the event log on the incognito
/// branch.
export interface PriorMessage {
    role: "user" | "assistant";
    content: string;
}

/// 2026-04-28 — synthesise a 3-5 word title for a conversation from
/// the first turn. Skipped silently on the server when the row is
/// already named or no inference backend is configured. Caller
/// should re-fetch `listThreads` after a successful (non-skipped)
/// response to surface the new label in the sidebar.
export interface GenerateTitleResponse {
    conversation_id: string;
    title: string | null;
    skipped: boolean;
}

export async function postGenerateTitle(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<GenerateTitleResponse> {
    return apiFetch<GenerateTitleResponse>(
        `/api/chats/${encodeURIComponent(conversationId)}/generate-title`,
        { method: "POST" },
        tokenAccessor,
    );
}

/// 2026-04-28 — hard-delete a thread. Server idempotent (200 even if
/// the row never existed). Caller is responsible for clearing local
/// state (active id, message list).
export async function deleteThread(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<{ conversation_id: string; existed: boolean }> {
    return apiFetch<{ conversation_id: string; existed: boolean }>(
        `/api/chats/${encodeURIComponent(conversationId)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

// ---- /api/admin/plugins -------------------------------------------

export interface PluginSummary {
    plugin_id: string;
    version: string;
    enabled: boolean;
    installed_at: number;
    updated_at: number;
    /// True when the plugin's manifest declares hooks the SPA
    /// renders a settings page for (today: any [[oauth_accounts]]).
    /// Drives the gear icon on the Plugins page row.
    has_settings_ui: boolean;
    /// Operator-facing one-liner from `[plugin].description` in the
    /// manifest. The Plugins page row renders this under the title
    /// with a single-line ellipsis truncation. May be omitted by
    /// older plugins that didn't fill it in.
    description?: string | null;
}

export interface PluginListResponse {
    plugins: PluginSummary[];
}

export async function listPlugins(
    tokenAccessor: () => string | null,
): Promise<PluginListResponse> {
    return apiFetch<PluginListResponse>("/api/admin/plugins", {}, tokenAccessor);
}

export async function enablePlugin(
    pluginId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/plugins/${encodeURIComponent(pluginId)}/enable`,
        { method: "POST" },
        tokenAccessor,
    );
}

export async function disablePlugin(
    pluginId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/plugins/${encodeURIComponent(pluginId)}/disable`,
        { method: "POST" },
        tokenAccessor,
    );
}

export async function uninstallPlugin(
    pluginId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/plugins/${encodeURIComponent(pluginId)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export interface InstallPluginResponse {
    plugin_id: string;
    version: string;
}

/**
 * Install (or upgrade) a plugin from a ZIP `File`. Sends raw
 * `application/zip` bytes — the backend handler accepts that
 * directly while multipart support lands later.
 *
 * `ifExisting`:
 *   * `"reject"` (default) — server returns 409 if a plugin with
 *     the same id is already installed. The SPA catches it and
 *     prompts the operator to replace.
 *   * `"upgrade"` — operator confirmed; tear down the old runtime
 *     and install the new ZIP in place. Per-plugin OAuth client +
 *     tokens (in `state_oauth_*`) survive untouched.
 */
export async function installPlugin(
    file: File,
    tokenAccessor: () => string | null,
    ifExisting: "reject" | "upgrade" = "reject",
): Promise<InstallPluginResponse> {
    const buf = await file.arrayBuffer();
    const headers: Record<string, string> = {
        "content-type": "application/zip",
    };
    const token = tokenAccessor();
    if (token) headers.authorization = `Bearer ${token}`;
    const url =
        ifExisting === "upgrade"
            ? "/api/admin/plugins/install?if_existing=upgrade"
            : "/api/admin/plugins/install";
    const resp = await fetch(url, {
        method: "POST",
        headers,
        body: buf,
    });
    if (!resp.ok) {
        let message = resp.statusText;
        try {
            const body = await resp.json();
            if (body?.error?.message) message = String(body.error.message);
        } catch {
            /* ignore */
        }
        const code: "unauthorized" | "conflict" | "bad_request" =
            resp.status === 401
                ? "unauthorized"
                : resp.status === 409
                  ? "conflict"
                  : "bad_request";
        throw new ApiError(code, message, resp.status);
    }
    return (await resp.json()) as InstallPluginResponse;
}

// ---- /api/admin/hardware ------------------------------------------

export interface HardwareGpu {
    pci_vendor_id?: string;
    pci_device_id?: string;
    vendor?: string;
    model?: string;
    [extra: string]: unknown;
}

export interface HardwareProfile {
    gpus?: HardwareGpu[];
    [extra: string]: unknown;
}

export async function getHardware(
    tokenAccessor: () => string | null,
): Promise<HardwareProfile> {
    return apiFetch<HardwareProfile>(
        "/api/admin/hardware",
        {},
        tokenAccessor,
    );
}

// ---- /api/admin/logs ----------------------------------------------

export interface LogEntry {
    ts_ms: number;
    level: string;
    target: string;
    conversation_id: string | null;
    plugin_id: string | null;
    message: string;
    fields: Record<string, unknown> | null;
}

export interface LogsResponse {
    entries: LogEntry[];
}

export interface LogsQuery {
    level?: string;
    plugin_id?: string;
    conversation_id?: string;
    since_ms?: number;
    limit?: number;
}

export async function getLogs(
    q: LogsQuery,
    tokenAccessor: () => string | null,
): Promise<LogsResponse> {
    const qs = new URLSearchParams();
    if (q.level) qs.set("level", q.level);
    if (q.plugin_id) qs.set("plugin_id", q.plugin_id);
    if (q.conversation_id) qs.set("conversation_id", q.conversation_id);
    if (q.since_ms !== undefined) qs.set("since_ms", String(q.since_ms));
    if (q.limit !== undefined) qs.set("limit", String(q.limit));
    const path = qs.toString()
        ? `/api/admin/logs?${qs.toString()}`
        : "/api/admin/logs";
    return apiFetch<LogsResponse>(path, {}, tokenAccessor);
}

// ---- /api/admin/eval/flags ----------------------------------------

export interface EvalFlag {
    id: number;
    label: string;
    conversation_id: string;
    seq: number;
    flagged_at: number;
    notes: string | null;
}

export interface EvalFlagsResponse {
    flags: EvalFlag[];
}

export async function getEvalFlags(
    label: string | undefined,
    tokenAccessor: () => string | null,
): Promise<EvalFlagsResponse> {
    const path = label
        ? `/api/admin/eval/flags?label=${encodeURIComponent(label)}`
        : "/api/admin/eval/flags";
    return apiFetch<EvalFlagsResponse>(path, {}, tokenAccessor);
}

// ---- /api/admin/runners/groups (Phase 16: supervisor-tracked) ------
//
// Live per-principal-group runners. The control plane manages one
// runner container per `(channel, principals)` group; the supervisor
// is the single source of truth.

export interface GroupRunnerView {
    group_id: string;
    controller_runner: boolean;
    /// "spawning" | "ready" | "stopping" | "dead"
    status: string;
    started_at: number;
    last_active_at: number;
    in_flight_turns: number;
    container_id: string | null;
}

export interface GroupRunnerListResponse {
    runners: GroupRunnerView[];
    idle_ttl_secs: number;
}

export async function listRunnerGroups(
    tokenAccessor: () => string | null,
): Promise<GroupRunnerListResponse> {
    return apiFetch<GroupRunnerListResponse>(
        "/api/admin/runners/groups",
        {},
        tokenAccessor,
    );
}

export async function restartRunnerGroup(
    groupId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/runners/groups/${encodeURIComponent(groupId)}/restart`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

export async function wipeRunnerGroup(
    groupId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/runners/groups/${encodeURIComponent(groupId)}/wipe`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

// -----------------------------------------------------------------
// Phase 2b — sidecar (companion-container) supervisor admin surface.
//
// One row per registered sidecar; status mirrors the supervisor's
// view (`stopped` / `starting` / `healthy` / `crash_looping` /
// `pulling` / `not_found`). Settings → Sidecars renders these.
// -----------------------------------------------------------------

export interface SidecarView {
    /// Globally-unique sidecar name (the manifest's
    /// `[[services]].name` for the entry that carried
    /// `[services.sidecar]`).
    name: string;
    plugin_id: string;
    /// "stopped" | "starting" | "healthy" | "crash_looping" |
    /// "pulling" | "not_found"
    status: string;
    restart_attempts: number;
    /// Loopback URL the supervisor would dispatch RPC against.
    /// Null until the first successful spawn.
    rpc_url: string | null;
}

export interface SidecarListResponse {
    sidecars: SidecarView[];
}

export async function listSidecars(
    tokenAccessor: () => string | null,
): Promise<SidecarListResponse> {
    return apiFetch<SidecarListResponse>(
        "/api/admin/sidecars",
        {},
        tokenAccessor,
    );
}

export async function resetSidecarAttempts(
    name: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/sidecars/${encodeURIComponent(name)}/reset`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

// ---- /api/admin/backends (Phase 8.5; replaces "deployments" CRUD) -

export type BackendPurpose = "Standard" | "Small" | "VoiceSTT" | "VoiceTTS";

/// Every purpose execlaw recognises. The Settings UI iterates this
/// so a missing slot renders as "not configured" instead of silently
/// disappearing.
export const BACKEND_PURPOSES: ReadonlyArray<BackendPurpose> = [
    "Standard",
    "Small",
    "VoiceSTT",
    "VoiceTTS",
];

export type BackendMode = "external" | "managed";

export interface BackendView {
    purpose: BackendPurpose;
    inference_backend: string;
    model_spec: Record<string, unknown>;
    gpu_id: string | null;
    endpoint: string | null;
    notes: string | null;
    /// Phase-8.8: whether reasoning mode is engaged on this
    /// backend. Server-controlled — only the Standard purpose
    /// retains a true value; Small / Voice* always come back as
    /// false.
    reasoning_enabled: boolean;
    /// True when this purpose accepts a reasoning_enabled value.
    /// The SPA shows the toggle only when this is true.
    supports_reasoning_toggle: boolean;
    /// Phase 12 — lifecycle ownership. "external" (operator URL) or
    /// "managed" (control plane spawns the container). Pre-Phase-12
    /// rows default to "external" via the migration.
    mode: BackendMode;
    created_at: number;
    updated_at: number;
}

export type BackendStatus =
    | "Pulling"
    | "Starting"
    | "Healthy"
    | "CrashLooping"
    | "Stopped"
    | "NotFound";

export type BackendStage =
    | "Idle"
    | "DownloadingModel"
    | "PullingImage"
    | "ContainerStarting"
    | "LoadingModel"
    | "Healthy"
    | "Failed";

export interface DownloadProgress {
    bytes_downloaded: number;
    total_bytes: number;
    file_idx: number;
    file_count: number;
}

export interface BackendStatusResponse {
    purpose: BackendPurpose;
    mode: BackendMode;
    status: BackendStatus;
    endpoint: string | null;
    restart_attempts: number;
    /// False when the supervisor isn't wired (e.g. dev build,
    /// Docker daemon unreachable). The SPA renders a "Docker
    /// unreachable" notice when the row is managed and this is
    /// false.
    supervisor_available: boolean;
    /// Higher-resolution lifecycle phase. The original `status`
    /// enum has only 6 values; `stage` subdivides "Starting" into
    /// PullingImage / ContainerStarting / LoadingModel so the SPA
    /// can render meaningful copy during long warm-ups.
    stage: BackendStage;
    /// Wall-clock seconds since the most recent successful spawn.
    /// `null` when no spawn has happened yet (Idle / external).
    elapsed_secs: number | null;
    /// Last meaningful log line from the running container.
    /// Surfaced in the status pill so the operator can see what
    /// the service is doing right now without opening the logs
    /// modal.
    last_log_line: string | null;
    /// In-flight HF model download progress. Populated only while
    /// stage = `DownloadingModel`.
    download_progress: DownloadProgress | null;
}

// ---- /api/admin/settings/hf-cache (Phase 14.C) -------------------

export interface HfCacheView {
    secondary_paths: string[];
    requires_restart: boolean;
}

export async function getHfCache(
    tokenAccessor: () => string | null,
): Promise<HfCacheView> {
    return apiFetch<HfCacheView>(
        "/api/admin/settings/hf-cache",
        {},
        tokenAccessor,
    );
}

export async function putHfCache(
    body: { secondary_paths: string[] },
    tokenAccessor: () => string | null,
): Promise<HfCacheView> {
    return apiFetch<HfCacheView>(
        "/api/admin/settings/hf-cache",
        { method: "PUT", body },
        tokenAccessor,
    );
}

/// One entry per purpose, regardless of whether the operator has
/// configured a backend yet. `configured = false` means
/// `backend = null` and the SPA should render an "Add backend"
/// affordance for that slot.
export interface BackendListEntry {
    purpose: BackendPurpose;
    configured: boolean;
    backend: BackendView | null;
}

export interface BackendListResponse {
    backends: BackendListEntry[];
}

export interface UpsertBackendRequest {
    inference_backend: string;
    model_spec: unknown;
    gpu_id?: string | null;
    endpoint?: string | null;
    notes?: string | null;
    /// Phase-8.8: ignored by the server for purposes that don't
    /// support reasoning (the field is silently zeroed). Send
    /// freely; let the server enforce the Standard-only rule.
    reasoning_enabled?: boolean;
    /// Phase 12 — lifecycle ownership. Defaults to "external" on
    /// the server when omitted, so older clients keep working.
    mode?: BackendMode;
}

export async function listBackends(
    tokenAccessor: () => string | null,
): Promise<BackendListResponse> {
    return apiFetch<BackendListResponse>(
        "/api/admin/backends",
        {},
        tokenAccessor,
    );
}

export async function upsertBackend(
    purpose: BackendPurpose,
    body: UpsertBackendRequest,
    tokenAccessor: () => string | null,
): Promise<BackendView> {
    return apiFetch<BackendView>(
        `/api/admin/backends/${encodeURIComponent(purpose)}`,
        { method: "PUT", body },
        tokenAccessor,
    );
}

export async function clearBackend(
    purpose: BackendPurpose,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/backends/${encodeURIComponent(purpose)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

/// Phase 12 — supervisor status for the SPA's mode pill. Polled by
/// the BackendsPage when at least one row is in managed mode.
export async function getBackendStatus(
    purpose: BackendPurpose,
    tokenAccessor: () => string | null,
): Promise<BackendStatusResponse> {
    return apiFetch<BackendStatusResponse>(
        `/api/admin/backends/${encodeURIComponent(purpose)}/status`,
        {},
        tokenAccessor,
    );
}

/// Force-restart a managed backend. 503 when the supervisor isn't
/// wired (Docker unreachable).
export async function restartBackend(
    purpose: BackendPurpose,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/backends/${encodeURIComponent(purpose)}/restart`,
        { method: "POST" },
        tokenAccessor,
    );
}

/// Phase 14 follow-up — fetch the last `lines` lines of the
/// managed container's stdout+stderr. Returns the cached
/// CrashLooping tail when the container has already been removed
/// by the supervisor, so the operator can see WHY it died.
export interface BackendLogsResponse {
    purpose: BackendPurpose;
    logs: string;
    supervisor_available: boolean;
    /// True when the snapshot came from the supervisor's cached
    /// CrashLooping tail (live container is gone). The SPA prefixes
    /// the modal title with "(crashed)" so the operator knows
    /// they're looking at post-mortem output.
    from_cache: boolean;
}

export async function getBackendLogs(
    purpose: BackendPurpose,
    lines: number | undefined,
    tokenAccessor: () => string | null,
): Promise<BackendLogsResponse> {
    const path =
        lines !== undefined
            ? `/api/admin/backends/${encodeURIComponent(purpose)}/logs?lines=${lines}`
            : `/api/admin/backends/${encodeURIComponent(purpose)}/logs`;
    return apiFetch<BackendLogsResponse>(path, {}, tokenAccessor);
}

// ---- Phase 13.B.1 — backend wizard presets ------------------------

/// One configurable knob a preset exposes (e.g. Whisper model size).
/// `kind` is the discriminator the SPA uses to pick a renderer; today
/// only "model_size" and "model" ship.
export interface PresetField {
    kind: string;
    label: string;
    choices: string[];
    default: string;
    /// Server-side template — purely informational on the SPA. The
    /// server materialises the spec on save; we just round-trip the
    /// kind+value pair.
    arg_template: string;
}

export interface BackendPreset {
    id: string;
    purpose: BackendPurpose;
    /// PluginId of the inference plugin that runs this preset. The
    /// SPA writes this verbatim into `inference_backend` on save —
    /// no client-side guessing from the preset id (audit closure
    /// for 13.B.1).
    inference_backend: string;
    name: string;
    description: string;
    image: string;
    container_port: number;
    /// "nvidia" | "intel" | "cpu" — drives the preset's badge + the
    /// recommended-card highlight.
    vendor: string;
    default_args: string[];
    fields: PresetField[];
}

export interface PresetWithFlag extends BackendPreset {
    /// True when this preset's vendor matches the host's detected
    /// hardware. The wizard pre-selects the recommended card.
    recommended: boolean;
}

export interface PresetsResponse {
    purpose: BackendPurpose;
    /// "nvidia" | "intel" | "amd" — empty array when no GPU was
    /// detected. The wizard uses this for the "Detected: NVIDIA"
    /// header badge.
    detected_vendors: string[];
    presets: PresetWithFlag[];
}

/// Phase 13.B.1 — fetch the curated preset list for a purpose, with
/// per-preset `recommended` flags driven by a fresh sysfs Tier-1 scan.
export async function listBackendPresets(
    purpose: BackendPurpose,
    tokenAccessor: () => string | null,
): Promise<PresetsResponse> {
    const url = `/api/admin/backends/presets?purpose=${encodeURIComponent(purpose)}`;
    return apiFetch<PresetsResponse>(url, {}, tokenAccessor);
}

// ---- /api/admin/plugins/ui_panels ---------------------------------

export interface UiPanelSummary {
    plugin_id: string;
    mount: string;
    entry: string;
}

export interface UiPanelListResponse {
    panels: UiPanelSummary[];
}

export async function listUiPanels(
    tokenAccessor: () => string | null,
): Promise<UiPanelListResponse> {
    return apiFetch<UiPanelListResponse>(
        "/api/admin/plugins/ui_panels",
        {},
        tokenAccessor,
    );
}

// ---- /api/admin/users (multi-controller) --------------------------

export type UserRole = "controller" | "operator" | "viewer";

export interface UserView {
    user_id: string;
    username: string;
    display_name: string;
    email: string | null;
    role: UserRole;
    created_at: number;
    last_login_at: number | null;
}

export interface UserListResponse {
    users: UserView[];
}

export interface InviteUserRequest {
    username: string;
    display_name: string;
    initial_password: string;
    role: UserRole;
    email?: string;
}

export async function listUsers(
    tokenAccessor: () => string | null,
): Promise<UserListResponse> {
    return apiFetch<UserListResponse>("/api/admin/users", {}, tokenAccessor);
}

export async function inviteUser(
    body: InviteUserRequest,
    tokenAccessor: () => string | null,
): Promise<UserView> {
    return apiFetch<UserView>(
        "/api/admin/users/invite",
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function deleteUser(
    userId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/users/${encodeURIComponent(userId)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

// ---- /api/admin/me/password + /api/admin/users/.../password ------

export interface ChangeMyPasswordRequest {
    current_password: string;
    new_password: string;
}

export interface ResetUserPasswordRequest {
    new_password: string;
}

/// Self-rotate the operator's password. Requires the current
/// password as proof of identity.
export async function changeMyPassword(
    body: ChangeMyPasswordRequest,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        "/api/admin/me/password",
        { method: "POST", body },
        tokenAccessor,
    );
}

/// Controller-only reset for another user. The server refuses if
/// the target is the caller themselves — use `changeMyPassword`
/// for that.
export async function resetUserPassword(
    userId: string,
    body: ResetUserPasswordRequest,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/users/${encodeURIComponent(userId)}/password`,
        { method: "POST", body },
        tokenAccessor,
    );
}

// ---- /api/admin/webauthn (Phase 7e second-factor) -----------------

/// Each row from `state_webauthn_credentials`, public surface (no
/// `passkey_json` blob — that's an implementation detail of the
/// relying-party crate).
export interface WebauthnCredentialView {
    credential_id: string;
    label: string;
    created_at: number;
    last_used_at: number | null;
}

export interface WebauthnCredentialListResponse {
    credentials: WebauthnCredentialView[];
}

export interface WebauthnRegisterBeginResponse {
    ceremony_id: string;
    /// Opaque PublicKeyCredentialCreationOptions JSON. The SPA
    /// passes this through `coerceCreationOptions()` and feeds the
    /// result to `navigator.credentials.create()`.
    options: unknown;
}

export interface WebauthnAssertBeginResponse {
    webauthn_required: true;
    ceremony_id: string;
    options: unknown;
}

/// Discriminated by `webauthn_required`. The login route's two
/// outcomes share the same HTTP status (200) — the SPA branches on
/// this flag.
export type LoginOutcome =
    | {
          webauthn_required: false;
          access_token: string;
          refresh_token: string;
      }
    | WebauthnAssertBeginResponse;

export async function listWebauthnCredentials(
    tokenAccessor: () => string | null,
): Promise<WebauthnCredentialListResponse> {
    return apiFetch<WebauthnCredentialListResponse>(
        "/api/admin/webauthn/credentials",
        {},
        tokenAccessor,
    );
}

export async function deleteWebauthnCredential(
    credentialId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/webauthn/credentials/${encodeURIComponent(credentialId)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export async function beginWebauthnRegistration(
    label: string,
    tokenAccessor: () => string | null,
): Promise<WebauthnRegisterBeginResponse> {
    return apiFetch<WebauthnRegisterBeginResponse>(
        "/api/admin/webauthn/register/begin",
        { method: "POST", body: { label } },
        tokenAccessor,
    );
}

export async function finishWebauthnRegistration(
    ceremonyId: string,
    credential: unknown,
    tokenAccessor: () => string | null,
): Promise<WebauthnCredentialView> {
    return apiFetch<WebauthnCredentialView>(
        "/api/admin/webauthn/register/finish",
        {
            method: "POST",
            body: { ceremony_id: ceremonyId, credential },
        },
        tokenAccessor,
    );
}

/// Finishes an in-flight login ceremony with the assertion produced
/// by `navigator.credentials.get()`. Returns the standard token pair
/// shape so callers feed it into `signIn(pair)` exactly like the
/// password-only path.
export async function finishWebauthnLogin(
    ceremonyId: string,
    credential: unknown,
): Promise<{ access_token: string; refresh_token: string }> {
    return apiFetch<{ access_token: string; refresh_token: string }>(
        "/api/login/webauthn/finish",
        {
            method: "POST",
            body: { ceremony_id: ceremonyId, credential },
        },
        () => null, // unauthenticated route — caller has no token yet
    );
}

// ---- /api/admin/tools (Phase 8a per-tool trust-class allowlist) ---

export type ToolSource = "builtin" | "plugin" | "mcp";

export interface ToolView {
    tool_name: string;
    source: ToolSource;
    source_id: string | null;
    enabled: boolean;
    allowed_classes: string[];
    description: string | null;
    first_seen_at: number;
    last_seen_at: number;
    removed_at: number | null;
}

export interface ToolListResponse {
    tools: ToolView[];
}

export interface UpdateToolPolicyRequest {
    enabled: boolean;
    /// Trust-class allowlist. Server rejects unknown strings with 400.
    allowed_classes: string[];
}

export async function listTools(
    tokenAccessor: () => string | null,
): Promise<ToolListResponse> {
    return apiFetch<ToolListResponse>("/api/admin/tools", {}, tokenAccessor);
}

export async function updateToolPolicy(
    toolName: string,
    body: UpdateToolPolicyRequest,
    tokenAccessor: () => string | null,
): Promise<ToolView> {
    return apiFetch<ToolView>(
        `/api/admin/tools/${encodeURIComponent(toolName)}`,
        { method: "PATCH", body },
        tokenAccessor,
    );
}

// ---- /api/admin/skills (Phase B.3 — top-level Skills page) --------

export type SkillState = "trial" | "stable" | "archived";
export type SkillRegistrationKind = "authored" | "shipped" | "registered";

export interface SkillListEntry {
    name: string;
    description: string;
    state: SkillState;
    version: number;
    registration_kind: SkillRegistrationKind;
    source: string;
    owning_plugin_id: string | null;
    updated_at: number;
}

export interface SkillListResponse {
    skills: SkillListEntry[];
}

export interface SkillDetail {
    name: string;
    description: string;
    state: SkillState;
    registration_kind: SkillRegistrationKind;
    source: string;
    owning_plugin_id: string | null;
    current_version: number;
    body_md: string;
    frontmatter_json: string;
    authored_by: string;
    authored_at: number;
    created_at: number;
    updated_at: number;
    archived_at: number | null;
    resource_paths: string[];
}

export async function listSkills(
    tokenAccessor: () => string | null,
    options?: { includeArchived?: boolean },
): Promise<SkillListResponse> {
    const qs = options?.includeArchived ? "?include_archived=true" : "";
    return apiFetch<SkillListResponse>(
        `/api/admin/skills${qs}`,
        {},
        tokenAccessor,
    );
}

export async function getSkill(
    name: string,
    tokenAccessor: () => string | null,
): Promise<SkillDetail> {
    return apiFetch<SkillDetail>(
        `/api/admin/skills/${encodeURIComponent(name)}`,
        {},
        tokenAccessor,
    );
}

export async function promoteSkill(
    name: string,
    notes: string | null,
    tokenAccessor: () => string | null,
): Promise<SkillDetail> {
    return apiFetch<SkillDetail>(
        `/api/admin/skills/${encodeURIComponent(name)}/promote`,
        { method: "POST", body: { notes } },
        tokenAccessor,
    );
}

export async function archiveSkill(
    name: string,
    tokenAccessor: () => string | null,
): Promise<SkillDetail> {
    return apiFetch<SkillDetail>(
        `/api/admin/skills/${encodeURIComponent(name)}/archive`,
        { method: "POST" },
        tokenAccessor,
    );
}

// ---- /api/admin/skills/config (Phase C — auto-capture worker) ----

export interface SkillsConfigView {
    auto_capture_enabled: boolean;
    auto_capture_min_tool_calls: number;
    auto_capture_dry_run: boolean;
    reuse_update_enabled: boolean;
    updated_at: number;
}

export interface UpdateSkillsConfigRequest {
    auto_capture_enabled?: boolean;
    auto_capture_min_tool_calls?: number;
    auto_capture_dry_run?: boolean;
    reuse_update_enabled?: boolean;
}

export async function getSkillsConfig(
    tokenAccessor: () => string | null,
): Promise<SkillsConfigView> {
    return apiFetch<SkillsConfigView>(
        "/api/admin/skills/config",
        {},
        tokenAccessor,
    );
}

export async function putSkillsConfig(
    body: UpdateSkillsConfigRequest,
    tokenAccessor: () => string | null,
): Promise<SkillsConfigView> {
    return apiFetch<SkillsConfigView>(
        "/api/admin/skills/config",
        { method: "PUT", body },
        tokenAccessor,
    );
}

// ---- Phase D.1: edit body, version history, proposals ----

export interface UpdateSkillBodyRequest {
    description: string;
    body_md: string;
    frontmatter_json?: string;
    promotion_notes?: string | null;
}

export async function updateSkillBody(
    name: string,
    body: UpdateSkillBodyRequest,
    tokenAccessor: () => string | null,
): Promise<SkillDetail> {
    return apiFetch<SkillDetail>(
        `/api/admin/skills/${encodeURIComponent(name)}`,
        { method: "PUT", body },
        tokenAccessor,
    );
}

export interface SkillVersionView {
    version: number;
    description: string;
    body_md: string;
    frontmatter_json: string;
    authored_by: string;
    authored_at: number;
    promotion_notes: string | null;
    parent_version: number | null;
}

export async function listSkillVersions(
    name: string,
    tokenAccessor: () => string | null,
): Promise<{ versions: SkillVersionView[] }> {
    return apiFetch<{ versions: SkillVersionView[] }>(
        `/api/admin/skills/${encodeURIComponent(name)}/versions`,
        {},
        tokenAccessor,
    );
}

export type ProposalStateFilter = "pending" | "approved" | "rejected" | "superseded" | "all";
export type ProposalKind = "new_skill" | "version_fork";
export type ProposalState = "pending" | "approved" | "rejected" | "superseded";

export interface SkillProposalView {
    id: number;
    kind: ProposalKind;
    target_skill_id: number | null;
    proposed_name: string;
    description: string;
    body_md: string;
    frontmatter_json: string;
    source_run_id: string;
    trajectory_summary: string | null;
    tool_calls_observed: number;
    state: ProposalState;
    promoted_skill_id: number | null;
    promoted_version_id: number | null;
    created_at: number;
    reviewed_at: number | null;
    reviewer: string | null;
    decision_notes: string | null;
}

export async function listSkillProposals(
    state: ProposalStateFilter,
    tokenAccessor: () => string | null,
): Promise<{ proposals: SkillProposalView[] }> {
    return apiFetch<{ proposals: SkillProposalView[] }>(
        `/api/admin/skills/proposals?state=${state}`,
        {},
        tokenAccessor,
    );
}

export async function approveSkillProposal(
    id: number,
    notes: string | null,
    tokenAccessor: () => string | null,
): Promise<SkillProposalView> {
    return apiFetch<SkillProposalView>(
        `/api/admin/skills/proposals/${id}/approve`,
        { method: "POST", body: { notes } },
        tokenAccessor,
    );
}

export async function rejectSkillProposal(
    id: number,
    notes: string | null,
    tokenAccessor: () => string | null,
): Promise<SkillProposalView> {
    return apiFetch<SkillProposalView>(
        `/api/admin/skills/proposals/${id}/reject`,
        { method: "POST", body: { notes } },
        tokenAccessor,
    );
}

// ---- /api/admin/mcp/servers (Phase 8c MCP integration) ------------

export type McpTransport = "stdio" | "streamable_http";

export type McpServerStatus =
    | "idle"
    | "connected"
    | "disconnected"
    | "error";

export interface McpServerView {
    id: string;
    display_name: string;
    transport: McpTransport;
    command: string | null;
    args: string[];
    env: Record<string, string>;
    cwd: string | null;
    url: string | null;
    auth_secret_ref: string | null;
    enabled: boolean;
    default_allowed_classes: string[];
    status: McpServerStatus;
    last_error: string | null;
    created_at: number;
    updated_at: number;
}

export interface McpServerListResponse {
    servers: McpServerView[];
}

export interface McpServerWriteRequest {
    id: string;
    display_name: string;
    transport: McpTransport;
    command?: string | null;
    args?: string[];
    env?: Record<string, string>;
    cwd?: string | null;
    url?: string | null;
    auth_secret_ref?: string | null;
    enabled?: boolean;
    default_allowed_classes?: string[];
}

export async function listMcpServers(
    tokenAccessor: () => string | null,
): Promise<McpServerListResponse> {
    return apiFetch<McpServerListResponse>(
        "/api/admin/mcp/servers",
        {},
        tokenAccessor,
    );
}

export async function createMcpServer(
    body: McpServerWriteRequest,
    tokenAccessor: () => string | null,
): Promise<McpServerView> {
    return apiFetch<McpServerView>(
        "/api/admin/mcp/servers",
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function updateMcpServer(
    id: string,
    body: McpServerWriteRequest,
    tokenAccessor: () => string | null,
): Promise<McpServerView> {
    return apiFetch<McpServerView>(
        `/api/admin/mcp/servers/${encodeURIComponent(id)}`,
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function deleteMcpServer(
    id: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/mcp/servers/${encodeURIComponent(id)}/delete`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

// ---- /api/admin/audit ---------------------------------------------

export interface AuditEntry {
    id: number;
    ts: number;
    actor: string;
    table_name: string;
    row_id: string;
    old_json: unknown;
    new_json: unknown;
}

export interface AuditResponse {
    entries: AuditEntry[];
}

export async function getAuditEntries(
    sinceTs: number | undefined,
    limit: number | undefined,
    tokenAccessor: () => string | null,
): Promise<AuditResponse> {
    const qs = new URLSearchParams();
    if (sinceTs !== undefined) qs.set("since_ts", String(sinceTs));
    if (limit !== undefined) qs.set("limit", String(limit));
    const path = qs.toString()
        ? `/api/admin/audit?${qs.toString()}`
        : "/api/admin/audit";
    return apiFetch<AuditResponse>(path, {}, tokenAccessor);
}

// ---- /api/admin/principals + /api/admin/approvals -----------------

export interface PrincipalIdentifier {
    transport: string;
    handle: string;
}

export interface PrincipalSummary {
    id: string;
    trust_class: string;
    display_name: string | null;
    first_seen: number;
    last_seen: number | null;
    identifiers: PrincipalIdentifier[];
}

export interface PrincipalListResponse {
    principals: PrincipalSummary[];
}

export async function listPrincipals(
    tokenAccessor: () => string | null,
): Promise<PrincipalListResponse> {
    return apiFetch<PrincipalListResponse>(
        "/api/admin/principals",
        {},
        tokenAccessor,
    );
}

export async function revokePrincipal(
    principalId: string,
    reason: string | undefined,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/principals/${encodeURIComponent(principalId)}/revoke`,
        { method: "POST", body: reason ? { reason } : {} },
        tokenAccessor,
    );
}

export interface PendingApprovalSummary {
    approval_id: string;
    conversation_id: string;
    sender_principal_id: string;
    original_text: string;
}

export interface PendingApprovalsResponse {
    approvals: PendingApprovalSummary[];
}

export async function listPendingApprovals(
    tokenAccessor: () => string | null,
): Promise<PendingApprovalsResponse> {
    return apiFetch<PendingApprovalsResponse>(
        "/api/admin/approvals",
        {},
        tokenAccessor,
    );
}

/**
 * Cold-contact approval verbs the controller can apply via
 * /api/admin/approvals/{id}/respond. The serde format on the Rust
 * side is snake_case so wire values match the variant names below.
 *
 *   - trust          → admit as KnownTrusted (full safe-tools)
 *   - trust_limited  → admit as KnownLimited (reply on this transport only)
 *   - claim_as_me    → controller's own handle; add to My identities,
 *                      reconcile, replay queued message as Controller turn
 *   - block          → flip principal to Blocked
 *   - ignore_once    → un-park without changing trust; future inbound
 *                      from this handle will re-prompt
 */
export type ApprovalVerb =
    | "trust"
    | "trust_limited"
    | "claim_as_me"
    | "block"
    | "ignore_once";

export interface RespondApprovalRequest {
    verb: ApprovalVerb;
    /** trust_limited only. */
    allowed_topics?: string[];
    /** Optional reason recorded with the approval. */
    reason?: string;
    /** Optional signed JWT supplied by the original approval-card link. */
    token?: string;
}

export async function respondApproval(
    approvalId: string,
    body: RespondApprovalRequest,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/approvals/${encodeURIComponent(approvalId)}/respond`,
        { method: "POST", body },
        tokenAccessor,
    );
}

// ---- /api/admin/personality (Phase 9 — §5.5) -----------------------

/**
 * Field names that the operator can override at any scope. Mirrors
 * `PersonalityField` on the Rust side. The SPA uses these strings
 * verbatim in the `override_fields` array on PUT requests.
 */
export const PERSONALITY_FIELDS = [
    "display_name",
    "role",
    "tone",
    "communication_style",
    "initiative",
    "about_agent",
    "about_controller",
    "custom_instructions",
    "voice_id",
] as const;
export type PersonalityField = (typeof PERSONALITY_FIELDS)[number];

export type PersonalityScopeKind = "default" | "conversation";

export interface PersonalityView {
    scope_kind: PersonalityScopeKind;
    /** "" for default scope; conversation_id for conversation overrides. */
    scope_ref: string;
    display_name: string;
    role: string;
    tone: string;
    communication_style: string;
    initiative: string;
    about_agent: string;
    about_controller: string;
    custom_instructions: string;
    voice_id: string | null;
    /** Field names the operator explicitly set at this scope. */
    override_fields: PersonalityField[];
    version: number;
    created_at: number;
    updated_at: number;
}

export interface PersonalityListResponse {
    default: PersonalityView;
    overrides: PersonalityView[];
}

export interface PersonalityPreviewResponse {
    conversation_id: string;
    system_prompt: string;
}

export interface UpsertPersonalityBody {
    display_name?: string;
    role?: string;
    tone?: string;
    communication_style?: string;
    initiative?: string;
    about_agent?: string;
    about_controller?: string;
    custom_instructions?: string;
    voice_id?: string | null;
    /**
     * Conversation-scope only. List of fields this scope explicitly
     * overrides; absent fields fall through to default. Ignored for
     * default-scope upserts (every field is implicitly overridden at
     * the default level).
     */
    override_fields?: PersonalityField[];
}

export async function listPersonality(
    tokenAccessor: () => string | null,
): Promise<PersonalityListResponse> {
    return apiFetch<PersonalityListResponse>(
        "/api/admin/personality",
        {},
        tokenAccessor,
    );
}

export async function upsertPersonalityDefault(
    body: UpsertPersonalityBody,
    tokenAccessor: () => string | null,
): Promise<PersonalityView> {
    return apiFetch<PersonalityView>(
        "/api/admin/personality/default",
        { method: "PUT", body },
        tokenAccessor,
    );
}

export async function getPersonalityConversation(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<PersonalityView> {
    return apiFetch<PersonalityView>(
        `/api/admin/personality/conversation/${encodeURIComponent(conversationId)}`,
        {},
        tokenAccessor,
    );
}

export async function upsertPersonalityConversation(
    conversationId: string,
    body: UpsertPersonalityBody,
    tokenAccessor: () => string | null,
): Promise<PersonalityView> {
    return apiFetch<PersonalityView>(
        `/api/admin/personality/conversation/${encodeURIComponent(conversationId)}`,
        { method: "PUT", body },
        tokenAccessor,
    );
}

export async function deletePersonalityConversation(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/personality/conversation/${encodeURIComponent(conversationId)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export async function previewPersonality(
    conversationId: string | null,
    tokenAccessor: () => string | null,
): Promise<PersonalityPreviewResponse> {
    const path =
        conversationId && conversationId.length > 0
            ? `/api/admin/personality/preview?conversation_id=${encodeURIComponent(conversationId)}`
            : "/api/admin/personality/preview";
    return apiFetch<PersonalityPreviewResponse>(path, {}, tokenAccessor);
}

// ---- /api/admin/alerts (Phase 9.1 — §10) ---------------------------

export type AlertSeverity = "Critical" | "Error" | "Warning" | "Info";
export type AlertStatus = "Firing" | "Acked" | "Resolved" | "Snoozed";

export interface AlertView {
    id: string;
    fingerprint: string;
    severity: AlertSeverity;
    source: string;
    title: string;
    detail: string | null;
    status: AlertStatus;
    first_seen_at: number;
    last_seen_at: number;
    occurrence_count: number;
    resolved_at: number | null;
    resolved_by: string | null;
    ack_at: number | null;
    ack_by: string | null;
    snooze_until: number | null;
    incident_id: string | null;
}

export interface AlertListResponse {
    alerts: AlertView[];
    firing_count: number;
}

export interface AlertCountResponse {
    firing_count: number;
}

/**
 * List alerts with optional status filter + cap.
 * `status` accepts comma-separated `Firing,Acked,Resolved,Snoozed`.
 * The server caps `limit` at 1000.
 */
export async function listAlerts(
    opts: { status?: AlertStatus[]; limit?: number },
    tokenAccessor: () => string | null,
): Promise<AlertListResponse> {
    const qs = new URLSearchParams();
    if (opts.status && opts.status.length > 0) {
        qs.set("status", opts.status.join(","));
    }
    if (opts.limit !== undefined) {
        qs.set("limit", String(opts.limit));
    }
    const path = qs.toString()
        ? `/api/admin/alerts?${qs.toString()}`
        : "/api/admin/alerts";
    return apiFetch<AlertListResponse>(path, {}, tokenAccessor);
}

/** Cheap firing-count query — used by the sidebar badge. */
export async function getAlertCount(
    tokenAccessor: () => string | null,
): Promise<AlertCountResponse> {
    return apiFetch<AlertCountResponse>(
        "/api/admin/alerts/count",
        {},
        tokenAccessor,
    );
}

export async function ackAlert(
    alertId: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/alerts/${encodeURIComponent(alertId)}/ack`,
        { method: "POST" },
        tokenAccessor,
    );
}

export async function resolveAlert(
    alertId: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/alerts/${encodeURIComponent(alertId)}/resolve`,
        { method: "POST" },
        tokenAccessor,
    );
}

// ---- /api/admin/trust-policy (Phase 9.2 — §2.6) --------------------

export type MinTrustHint = "Contact" | "Colleague" | "Organization";
export type MixedTrustPolicy = "min_wins";
export type AutoTrustClass = "KnownLimited" | "KnownTrusted";

export interface TrustPolicyView {
    auto_trust_contacts: boolean;
    min_trust_hint_for_auto_trust: MinTrustHint;
    /**
     * Trust class auto-admitted senders (plugin-vouched at or above
     * `min_trust_hint_for_auto_trust`) enter at. Defaults to
     * `KnownLimited` — agent can reply on the originating transport
     * but can't read/write memory or call non-trivial tools.
     */
    auto_trust_class: AutoTrustClass;
    mixed_trust_policy: MixedTrustPolicy;
    identity_plugin_order: string[];
    /** Duration string, e.g. "7d", "12h". */
    delegated_trust_default_ttl: string;
}

export async function getTrustPolicy(
    tokenAccessor: () => string | null,
): Promise<TrustPolicyView> {
    return apiFetch<TrustPolicyView>(
        "/api/admin/trust-policy",
        {},
        tokenAccessor,
    );
}

export async function putTrustPolicy(
    body: TrustPolicyView,
    tokenAccessor: () => string | null,
): Promise<TrustPolicyView> {
    return apiFetch<TrustPolicyView>(
        "/api/admin/trust-policy",
        { method: "PUT", body },
        tokenAccessor,
    );
}

// ---- /api/admin/me/identifiers (Phase 9.3 — §7.1) ------------------

export interface IdentifierView {
    transport: string;
    handle: string;
}

export interface MyIdentitiesResponse {
    controller_principal_id: string;
    identifiers: IdentifierView[];
}

export async function listMyIdentifiers(
    tokenAccessor: () => string | null,
): Promise<MyIdentitiesResponse> {
    return apiFetch<MyIdentitiesResponse>(
        "/api/admin/me/identifiers",
        {},
        tokenAccessor,
    );
}

export async function addMyIdentifier(
    transport: string,
    handle: string,
    tokenAccessor: () => string | null,
): Promise<MyIdentitiesResponse> {
    return apiFetch<MyIdentitiesResponse>(
        "/api/admin/me/identifiers",
        { method: "POST", body: { transport, handle } },
        tokenAccessor,
    );
}

export async function deleteMyIdentifier(
    transport: string,
    handle: string,
    tokenAccessor: () => string | null,
): Promise<MyIdentitiesResponse> {
    return apiFetch<MyIdentitiesResponse>(
        `/api/admin/me/identifiers/${encodeURIComponent(transport)}/${encodeURIComponent(handle)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

/// One transport the operator can bind a handle against. Built-in
/// entries (`web`, `voice`) come from the platform; plugin entries
/// reflect installed `[transport]` declarations. The list is
/// fetched live so the dropdown reflects the actual install state
/// — operators don't see "Signal" until the Signal plugin is
/// installed + enabled.
export interface AvailableTransportView {
    id: string;
    label: string;
    plugin_id?: string;
    handle_placeholder: string;
}

export interface AvailableTransportsResponse {
    transports: AvailableTransportView[];
}

export async function listAvailableTransports(
    tokenAccessor: () => string | null,
): Promise<AvailableTransportsResponse> {
    return apiFetch<AvailableTransportsResponse>(
        "/api/admin/me/transports",
        {},
        tokenAccessor,
    );
}

// ---- /api/admin/signal/* (Phase 8 — operator pairing UI) ------------

/// Live pairing/registration status for the supervised signal-cli
/// sidecar. The Settings → Plugin → Signal page polls this every
/// few seconds while the operator is on-page so a fresh QR scan
/// surfaces as a populated `registered_accounts` list without a
/// manual reload.
export interface SignalStatusResponse {
    /// Lower-cased supervisor status string (e.g. "starting",
    /// "healthy", "crashlooping"). The SPA chips this so the
    /// operator can correlate a stuck pairing flow with the
    /// underlying sidecar lifecycle.
    sidecar_status: string;
    /// `127.0.0.1:<port>` once the supervisor mints one. Surfaced
    /// in the chip's hover-detail, useful when triaging.
    sidecar_rpc_url: string | null;
    /// E.164 phone numbers signal-cli has on file. Empty list →
    /// "not paired yet"; the SPA renders the QR link affordance.
    registered_accounts: string[];
    /// Phone numbers persisted to the sidecar's host-side
    /// accounts.json. When this exceeds `registered_accounts` the
    /// running daemon's in-memory state has drifted from disk —
    /// signal-cli upstream bug after device-link succeeds. The SPA
    /// detects the gap and POSTs to /finalize-pairing to force a
    /// daemon restart that picks up the new account on cold load.
    accounts_on_disk: string[];
    /// Populated when the proxy hop to /v1/accounts failed. Lets
    /// the SPA render the actual error verbatim instead of a
    /// blank "no accounts."
    fetch_error: string | null;
}

export async function getSignalStatus(
    tokenAccessor: () => string | null,
): Promise<SignalStatusResponse> {
    return apiFetch<SignalStatusResponse>(
        "/api/admin/signal/status",
        {},
        tokenAccessor,
    );
}

/// Build a query-stringed URL for `/api/admin/signal/qrcodelink`
/// the SPA can shove directly into an `<img src>`. The endpoint
/// streams PNG bytes from the supervised sidecar's
/// `/v1/qrcodelink`. Bearer-token auth via the `?access_token=`
/// query-string fallback (the SPA can't stamp `Authorization`
/// headers on raw image src loads).
export function signalQrCodeLinkUrl(
    accessToken: string | null,
    deviceName?: string,
): string {
    const params = new URLSearchParams();
    if (deviceName && deviceName.trim().length > 0) {
        params.set("device_name", deviceName.trim());
    }
    if (accessToken) {
        params.set("access_token", accessToken);
    }
    const qs = params.toString();
    return `/api/admin/signal/qrcodelink${qs ? `?${qs}` : ""}`;
}

export async function unregisterSignalAccount(
    number: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/signal/accounts/${encodeURIComponent(number)}`,
        { method: "DELETE", rawText: true },
        tokenAccessor,
    );
}

/// Trigger the supervisor to restart the signal-cli sidecar so the
/// daemon re-reads accounts from disk. Workaround for an upstream
/// signal-cli bug that leaves the running daemon out of sync after
/// a successful device-link. The SPA auto-fires this when it sees
/// `accounts_on_disk` outpace `registered_accounts`.
export async function finalizeSignalPairing(
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        "/api/admin/signal/finalize-pairing",
        { method: "POST", rawText: true },
        tokenAccessor,
    );
}

// ---- /api/admin/routines (Phase 10 — §5.6) -------------------------

export type RoutineRunStatus = "Pending" | "Success" | "Failed" | "Skipped";

export interface RoutineView {
    id: string;
    name: string;
    schedule_cron: string;
    timezone: string;
    prompt: string;
    target_conversation_id: string | null;
    enabled: boolean;
    last_run_at: number | null;
    last_run_status: RoutineRunStatus | null;
    next_run_at: number | null;
    created_at: number;
    updated_at: number;
}

export interface RoutineListResponse {
    routines: RoutineView[];
}

export interface RoutineRunView {
    id: string;
    routine_id: string;
    fired_at: number;
    started_at: number | null;
    finished_at: number | null;
    status: RoutineRunStatus;
    error: string | null;
    conversation_id: string | null;
}

export interface RoutineRunListResponse {
    runs: RoutineRunView[];
}

export interface UpsertRoutineBody {
    name: string;
    schedule_cron: string;
    timezone?: string;
    prompt: string;
    target_conversation_id?: string | null;
    enabled?: boolean;
}

export interface RoutinePreviewResponse {
    next_fires_unix: number[];
}

export async function listRoutines(
    tokenAccessor: () => string | null,
): Promise<RoutineListResponse> {
    return apiFetch<RoutineListResponse>(
        "/api/admin/routines",
        {},
        tokenAccessor,
    );
}

export async function createRoutine(
    body: UpsertRoutineBody,
    tokenAccessor: () => string | null,
): Promise<RoutineView> {
    return apiFetch<RoutineView>(
        "/api/admin/routines",
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function updateRoutine(
    id: string,
    body: UpsertRoutineBody,
    tokenAccessor: () => string | null,
): Promise<RoutineView> {
    return apiFetch<RoutineView>(
        `/api/admin/routines/${encodeURIComponent(id)}`,
        { method: "PUT", body },
        tokenAccessor,
    );
}

export async function deleteRoutine(
    id: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/routines/${encodeURIComponent(id)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export async function runRoutineNow(
    id: string,
    tokenAccessor: () => string | null,
): Promise<RoutineRunView> {
    return apiFetch<RoutineRunView>(
        `/api/admin/routines/${encodeURIComponent(id)}/run-now`,
        { method: "POST" },
        tokenAccessor,
    );
}

export async function listRoutineRuns(
    id: string,
    limit: number | undefined,
    tokenAccessor: () => string | null,
): Promise<RoutineRunListResponse> {
    const path =
        limit !== undefined
            ? `/api/admin/routines/${encodeURIComponent(id)}/runs?limit=${limit}`
            : `/api/admin/routines/${encodeURIComponent(id)}/runs`;
    return apiFetch<RoutineRunListResponse>(path, {}, tokenAccessor);
}

export async function previewRoutine(
    schedule_cron: string,
    timezone: string,
    n: number,
    tokenAccessor: () => string | null,
): Promise<RoutinePreviewResponse> {
    return apiFetch<RoutinePreviewResponse>(
        "/api/admin/routines/preview",
        {
            method: "POST",
            body: { schedule_cron, timezone, n },
        },
        tokenAccessor,
    );
}

// ---- /api/logout ---------------------------------------------------

export async function postLogout(refreshToken: string | null): Promise<void> {
    await apiFetch<{ ok: boolean }>("/api/logout", {
        method: "POST",
        body: refreshToken ? { refresh_token: refreshToken } : {},
    });
}

// ---- /api/admin/settings/general (Phase 14 — bare-metal pivot) ----

export interface GeneralSettings {
    start_on_boot: boolean;
    bind_address: string;
    updated_at: number;
    /// Server contract: editing `bind_address` requires
    /// `execlaw service restart` to take effect. The SPA reads
    /// this flag rather than hardcoding the message so a future
    /// in-process rebind can flip it without an SPA change.
    bind_address_requires_restart: boolean;
    /// 2026-04-29 — global history retention. `0` = infinite (never
    /// delete). Other legal values are 30 / 60 / 90 / 120. Sweepers
    /// across the workspace consume this on each tick.
    history_retention_days: number;
}

export interface UpdateGeneralSettingsRequest {
    start_on_boot?: boolean;
    bind_address?: string;
    history_retention_days?: number;
}

/// Legal values for the history-retention dropdown. Mirrors the
/// `ALLOWED_RETENTION_DAYS` list in core's `retention.rs` plus `0`
/// for the "Infinite" option. Adding a value requires updating both.
export const HISTORY_RETENTION_OPTIONS: ReadonlyArray<
    { value: number; label: string }
> = [
    { value: 30, label: "30 days" },
    { value: 60, label: "60 days" },
    { value: 90, label: "90 days" },
    { value: 120, label: "120 days" },
    { value: 0, label: "Infinite (never delete)" },
];

export async function getGeneralSettings(
    tokenAccessor: () => string | null,
): Promise<GeneralSettings> {
    return apiFetch<GeneralSettings>(
        "/api/admin/settings/general",
        {},
        tokenAccessor,
    );
}

export async function updateGeneralSettings(
    body: UpdateGeneralSettingsRequest,
    tokenAccessor: () => string | null,
): Promise<GeneralSettings> {
    return apiFetch<GeneralSettings>(
        "/api/admin/settings/general",
        { method: "PUT", body },
        tokenAccessor,
    );
}

// ---- /api/admin/factory-reset ---------------------------------------

export interface FactoryResetResponse {
    tables_wiped: number;
    restart_recommended: boolean;
}

/// POSTs the literal `confirm` string the operator typed into the
/// danger-zone input. The server compares it against its own
/// constant — currently `"RESET"` — and 400s anything else. After a
/// 200, the SPA must sign-out immediately because the controller
/// row backing the JWT is gone.
export async function postFactoryReset(
    confirm: string,
    tokenAccessor: () => string | null,
): Promise<FactoryResetResponse> {
    return apiFetch<FactoryResetResponse>(
        "/api/admin/factory-reset",
        { method: "POST", body: { confirm } },
        tokenAccessor,
    );
}

// ---- /api/admin/settings/research (C3 — deep-research subsystem) ----

/// Mirrors `core::research::PhaseGates`. The dropdown on Settings →
/// Research uses these literal strings.
export type ResearchPhaseGates = "none" | "plan_only" | "every_phase";

export interface ResearchSettings {
    max_wall_clock_minutes: number;
    max_total_tokens: number;
    max_subqueries: number;
    parallel_workers: number;
    max_urls_per_subquery: number;
    max_pages_total: number;
    auto_cancel_after_idle_secs: number;
    phase_gates: ResearchPhaseGates;
    /// `null` means "inherit from Settings → Search."
    default_search_provider: string | null;
    updated_at: number;
}

export interface UpdateResearchSettingsRequest {
    max_wall_clock_minutes?: number;
    max_total_tokens?: number;
    max_subqueries?: number;
    parallel_workers?: number;
    max_urls_per_subquery?: number;
    max_pages_total?: number;
    auto_cancel_after_idle_secs?: number;
    phase_gates?: ResearchPhaseGates;
    /// Send `null` to clear (inherit). Omit to leave untouched.
    default_search_provider?: string | null;
}

export const RESEARCH_PHASE_GATE_OPTIONS: ReadonlyArray<
    { value: ResearchPhaseGates; label: string; description: string }
> = [
    {
        value: "plan_only",
        label: "Confirm after planning",
        description:
            "Pause once the planner finishes, before the gather phase fires. Default — gives you a one-click confirm before the expensive part.",
    },
    {
        value: "none",
        label: "No confirmations",
        description: "Auto-advance through every phase.",
    },
    {
        value: "every_phase",
        label: "Confirm between every phase",
        description: "Pause after planning, after gather, and before synthesize.",
    },
];

export async function getResearchSettings(
    tokenAccessor: () => string | null,
): Promise<ResearchSettings> {
    return apiFetch<ResearchSettings>(
        "/api/admin/settings/research",
        {},
        tokenAccessor,
    );
}

export async function updateResearchSettings(
    body: UpdateResearchSettingsRequest,
    tokenAccessor: () => string | null,
): Promise<ResearchSettings> {
    return apiFetch<ResearchSettings>(
        "/api/admin/settings/research",
        { method: "PUT", body },
        tokenAccessor,
    );
}

// ---- /api/admin/research/* (C6 — operator drill-down + badge) ----

export interface ResearchPlanStepView {
    query: string;
    rationale?: string | null;
}

export interface ResearchPlanView {
    thesis: string;
    steps: ResearchPlanStepView[];
}

export interface ResearchSourceView {
    url: string;
    title?: string | null;
    fetched_ok?: boolean;
    error?: string | null;
}

export interface ResearchNoteView {
    index: number;
    sub_query: string;
    state: "Pending" | "Running" | "Done" | "Failed";
    excerpt: string;
    sources: ResearchSourceView[];
    tokens_used?: number | null;
    error?: string | null;
}

export interface ResearchJobSummaryView {
    id: string;
    conversation_id: string;
    query: string;
    status:
        | "pending"
        | "planning"
        | "planned"
        | "gathering"
        | "synthesizing"
        | "complete"
        | "failed"
        | "cancelled";
    card_id: string | null;
    workspace_path: string | null;
    attachment_id: string | null;
    error: string | null;
    created_at: number;
    updated_at: number;
    started_at: number | null;
    finished_at: number | null;
    plan: ResearchPlanView | null;
    notes: ResearchNoteView[];
}

export interface ResearchJobsResponse {
    jobs: ResearchJobSummaryView[];
    count: number;
}

export interface ResearchJobReportResponse {
    job_id: string;
    report_markdown: string | null;
}

export interface ResearchActiveCountResponse {
    active_count: number;
    conversation_id: string | null;
}

export const RESEARCH_TERMINAL_STATUSES: ReadonlySet<
    ResearchJobSummaryView["status"]
> = new Set(["complete", "failed", "cancelled"]);

export async function listResearchJobs(
    tokenAccessor: () => string | null,
): Promise<ResearchJobsResponse> {
    return apiFetch<ResearchJobsResponse>(
        "/api/admin/research/jobs",
        {},
        tokenAccessor,
    );
}

export async function getResearchJob(
    jobId: string,
    tokenAccessor: () => string | null,
): Promise<ResearchJobSummaryView> {
    return apiFetch<ResearchJobSummaryView>(
        `/api/admin/research/jobs/${encodeURIComponent(jobId)}`,
        {},
        tokenAccessor,
    );
}

export async function getResearchReport(
    jobId: string,
    tokenAccessor: () => string | null,
): Promise<ResearchJobReportResponse> {
    return apiFetch<ResearchJobReportResponse>(
        `/api/admin/research/jobs/${encodeURIComponent(jobId)}/report`,
        {},
        tokenAccessor,
    );
}

export async function getResearchActiveCount(
    tokenAccessor: () => string | null,
    opts?: { conversationId?: string },
): Promise<ResearchActiveCountResponse> {
    const path = opts?.conversationId
        ? `/api/admin/research/active_count?conversation_id=${encodeURIComponent(opts.conversationId)}`
        : "/api/admin/research/active_count";
    return apiFetch<ResearchActiveCountResponse>(path, {}, tokenAccessor);
}

export interface ResearchAdvanceResponse {
    job_id: string;
    /// New status after the advance.
    status: ResearchJobSummaryView["status"];
    /// `true` when the request triggered a phase. `false` when the
    /// row was in a non-advanceable state (idempotent no-op).
    advanced: boolean;
}

export interface ResearchCancelResponse {
    job_id: string;
    /// `true` when the row flipped to Cancelled. `false` for
    /// already-terminal rows.
    cancelled: boolean;
}

export async function advanceResearchJob(
    jobId: string,
    tokenAccessor: () => string | null,
): Promise<ResearchAdvanceResponse> {
    return apiFetch<ResearchAdvanceResponse>(
        `/api/admin/research/jobs/${encodeURIComponent(jobId)}/advance`,
        { method: "POST" },
        tokenAccessor,
    );
}

export async function cancelResearchJob(
    jobId: string,
    tokenAccessor: () => string | null,
    reason?: string,
): Promise<ResearchCancelResponse> {
    return apiFetch<ResearchCancelResponse>(
        `/api/admin/research/jobs/${encodeURIComponent(jobId)}/cancel`,
        {
            method: "POST",
            body: { reason: reason ?? null },
        },
        tokenAccessor,
    );
}

// ---- /api/admin/setup/preflight (Phase 14 — first-run wizard) ----

export interface DockerStatus {
    available: boolean;
    version: string | null;
}

export interface DetectedGpu {
    /// Stringified `GpuId`. Server uses `<vendor_hex>:<device_or_card>`
    /// — opaque to the SPA. Used as the `gpu_id` value when saving
    /// the Standard backend if the operator picks a specific card.
    id: { 0: string } | string;
    vendor: "Nvidia" | "Intel" | "Amd" | "Unknown";
    pci_vendor_id: string;
    pci_device_id: string;
    /// Linux-only on bare-metal sysfs paths; empty on Windows/macOS
    /// hardware-query results.
    device_files?: string[];
    kernel_card_index?: number;
    /// Resolved SKU like "GeForce RTX 4090" or "Arc A770". Falls
    /// back to `null` on the legacy sysfs path. The setup wizard
    /// renders this as the badge label.
    model_name?: string | null;
    /// VRAM in MiB. Used by the model catalog to filter to entries
    /// that fit. `null` when unresolvable (Apple Silicon unified
    /// memory, sysfs path, etc.).
    memory_mb?: number | null;
}

export interface PreflightResponse {
    docker: DockerStatus;
    gpus: DetectedGpu[];
    /// Free space on the volume that hosts execlaw's HF model
    /// cache. `null` when the platform's free-space probe failed
    /// (rare). The setup wizard renders a "couldn't detect free
    /// space" warning in that case.
    disk_free_bytes?: number | null;
    /// Path the free-space probe was run against. Surfaced in the
    /// warning copy so the operator knows which volume to free up.
    disk_free_path?: string | null;
}

export async function getSetupPreflight(
    tokenAccessor: () => string | null,
): Promise<PreflightResponse> {
    return apiFetch<PreflightResponse>(
        "/api/admin/setup/preflight",
        {},
        tokenAccessor,
    );
}

// ---- /api/admin/oauth (Phase 9 — generic OAuth client + token mgmt) ----

export interface OauthClientView {
    plugin_id: string;
    account_name: string;
    provider: string;
    client_id: string;
    redirect_uri: string;
    scopes: string[];
    created_at: number;
    updated_at: number;
    /// True when tokens are present + non-expired.
    connected: boolean;
    account_email: string | null;
    /// Unix-seconds expiry of the current access_token, when present.
    token_expires_at: number | null;
}

export interface OauthClientsResponse {
    clients: OauthClientView[];
}

export interface UpsertOauthClientRequest {
    provider: string;
    client_id: string;
    /// Empty string preserves the persisted secret (form pre-population).
    client_secret: string;
    redirect_uri: string;
    scopes: string[];
}

export interface ConnectOauthResponse {
    authorize_url: string;
}

export async function listOauthClients(
    tokenAccessor: () => string | null,
): Promise<OauthClientsResponse> {
    return apiFetch<OauthClientsResponse>(
        "/api/admin/oauth/clients",
        {},
        tokenAccessor,
    );
}

export async function getOauthClient(
    pluginId: string,
    accountName: string,
    tokenAccessor: () => string | null,
): Promise<OauthClientView> {
    return apiFetch<OauthClientView>(
        `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}`,
        {},
        tokenAccessor,
    );
}

export async function upsertOauthClient(
    pluginId: string,
    accountName: string,
    req: UpsertOauthClientRequest,
    tokenAccessor: () => string | null,
): Promise<OauthClientView> {
    return apiFetch<OauthClientView>(
        `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}`,
        { method: "PUT", body: req },
        tokenAccessor,
    );
}

export async function deleteOauthClient(
    pluginId: string,
    accountName: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export async function connectOauth(
    pluginId: string,
    accountName: string,
    tokenAccessor: () => string | null,
): Promise<ConnectOauthResponse> {
    return apiFetch<ConnectOauthResponse>(
        `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}/connect`,
        { method: "POST" },
        tokenAccessor,
    );
}

export async function disconnectOauth(
    pluginId: string,
    accountName: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}/disconnect`,
        { method: "POST" },
        tokenAccessor,
    );
}


// ============================================================
// Settings → Search (search-provider registry)
// ============================================================

export interface SearchProviderView {
    kind: string;
    display_name: string;
    enabled: boolean;
    is_default: boolean;
    /// Per-kind config object. Shape varies by `kind`:
    ///   * `duckduckgo` → `{}`
    ///   * `searxng`    → `{ base_url: string }`
    ///   * `brave`      → `{ api_key: string }`
    config: Record<string, unknown>;
    created_at: number;
    updated_at: number;
}

export interface ListSearchProvidersResponse {
    providers: SearchProviderView[];
}

export interface UpsertSearchProviderRequest {
    kind: string;
    enabled: boolean;
    is_default: boolean;
    config: Record<string, unknown>;
}

export interface SearchTestRequest {
    query: string;
}

export interface SearchTestHit {
    title: string;
    url: string;
    snippet: string | null;
}

export interface SearchTestResponse {
    provider_id: string;
    results: SearchTestHit[];
    elapsed_ms: number;
}

export async function listSearchProviders(
    tokenAccessor: () => string | null,
): Promise<ListSearchProvidersResponse> {
    return apiFetch<ListSearchProvidersResponse>(
        "/api/admin/search/providers",
        {},
        tokenAccessor,
    );
}

export async function upsertSearchProvider(
    req: UpsertSearchProviderRequest,
    tokenAccessor: () => string | null,
): Promise<SearchProviderView> {
    return apiFetch<SearchProviderView>(
        "/api/admin/search/providers",
        { method: "POST", body: req },
        tokenAccessor,
    );
}

export async function deleteSearchProvider(
    kind: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `/api/admin/search/providers/${encodeURIComponent(kind)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export async function setDefaultSearchProvider(
    kind: string,
    tokenAccessor: () => string | null,
): Promise<SearchProviderView> {
    return apiFetch<SearchProviderView>(
        "/api/admin/search/providers/default",
        { method: "POST", body: { kind } },
        tokenAccessor,
    );
}

export async function testSearchProvider(
    kind: string,
    query: string,
    tokenAccessor: () => string | null,
): Promise<SearchTestResponse> {
    return apiFetch<SearchTestResponse>(
        `/api/admin/search/providers/${encodeURIComponent(kind)}/test`,
        { method: "POST", body: { query } },
        tokenAccessor,
    );
}
