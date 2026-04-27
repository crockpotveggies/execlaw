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

// ---- /api/admin/plugins -------------------------------------------

export interface PluginSummary {
    plugin_id: string;
    version: string;
    enabled: boolean;
    installed_at: number;
    updated_at: number;
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
 * Install a plugin from a ZIP `File` (browser File API). Sends raw
 * `application/zip` bytes — the Phase-2 backend handler accepts that
 * directly while multipart support lands later.
 */
export async function installPlugin(
    file: File,
    tokenAccessor: () => string | null,
): Promise<InstallPluginResponse> {
    const buf = await file.arrayBuffer();
    const headers: Record<string, string> = {
        "content-type": "application/zip",
    };
    const token = tokenAccessor();
    if (token) headers.authorization = `Bearer ${token}`;
    const resp = await fetch("/api/admin/plugins/install", {
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
        throw new ApiError(
            resp.status === 401 ? "unauthorized" : "bad_request",
            message,
            resp.status,
        );
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

// ---- /api/admin/runners (Phase 8.5 view-only + restart) ------------

export type RunnerModality = "Text" | "Voice";

export interface RunnerView {
    conversation_id: string;
    principal_label: string | null;
    modality: RunnerModality;
    controller_runner: boolean;
    started_at: number;
    last_active_at: number;
    in_flight: boolean;
    turn_count: number;
    restart_pending: boolean;
    /// Seconds until the idle reaper drops this entry; null for the
    /// controller runner and any in-flight runner.
    idle_secs_remaining: number | null;
}

export interface RunnerListResponse {
    runners: RunnerView[];
    /// Idle TTL the reaper applies to non-controller runners. Surfaced
    /// so the SPA can label the row's countdown.
    idle_ttl_secs: number;
}

export async function listRunners(
    tokenAccessor: () => string | null,
): Promise<RunnerListResponse> {
    return apiFetch<RunnerListResponse>(
        "/api/admin/runners",
        {},
        tokenAccessor,
    );
}

export async function restartRunner(
    conversationId: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/runners/${encodeURIComponent(conversationId)}/restart`,
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

export interface RespondApprovalRequest {
    /** "Trust" | "TrustLimited" | "Block" | "TrustOnce" — server-defined. */
    verb: string;
    /** TrustLimited only. */
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

export interface TrustPolicyView {
    auto_trust_contacts: boolean;
    min_trust_hint_for_auto_trust: MinTrustHint;
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
}

export interface UpdateGeneralSettingsRequest {
    start_on_boot?: boolean;
    bind_address?: string;
}

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
