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

// ---- /api/admin/deployments ---------------------------------------

export type DeploymentPurpose =
    | "Standard"
    | "Reasoning"
    | "Guardrail"
    | "VoiceSTT"
    | "VoiceTTS";

export interface DeploymentView {
    id: string;
    purpose: DeploymentPurpose;
    inference_backend: string;
    model_spec: Record<string, unknown>;
    gpu_id: string | null;
    endpoint: string | null;
    is_default: boolean;
    active: boolean;
    notes: string | null;
    created_at: number;
    updated_at: number;
}

export interface DeploymentListResponse {
    deployments: DeploymentView[];
}

export interface CreateDeploymentRequest {
    id?: string;
    purpose: DeploymentPurpose;
    inference_backend: string;
    model_spec: unknown;
    gpu_id?: string | null;
    endpoint?: string | null;
    is_default?: boolean;
    active?: boolean;
    notes?: string | null;
}

export interface UpdateDeploymentRequest {
    purpose?: DeploymentPurpose;
    inference_backend?: string;
    model_spec?: unknown;
    /** Three-valued: missing / null / value. */
    gpu_id?: string | null;
    endpoint?: string | null;
    is_default?: boolean;
    active?: boolean;
    notes?: string | null;
}

export async function listDeployments(
    tokenAccessor: () => string | null,
): Promise<DeploymentListResponse> {
    return apiFetch<DeploymentListResponse>(
        "/api/admin/deployments",
        {},
        tokenAccessor,
    );
}

export async function createDeployment(
    body: CreateDeploymentRequest,
    tokenAccessor: () => string | null,
): Promise<DeploymentView> {
    return apiFetch<DeploymentView>(
        "/api/admin/deployments",
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function updateDeployment(
    id: string,
    body: UpdateDeploymentRequest,
    tokenAccessor: () => string | null,
): Promise<DeploymentView> {
    return apiFetch<DeploymentView>(
        `/api/admin/deployments/${encodeURIComponent(id)}`,
        { method: "PATCH", body },
        tokenAccessor,
    );
}

export async function deleteDeployment(
    id: string,
    tokenAccessor: () => string | null,
): Promise<unknown> {
    return apiFetch(
        `/api/admin/deployments/${encodeURIComponent(id)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
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

// ---- /api/logout ---------------------------------------------------

export async function postLogout(refreshToken: string | null): Promise<void> {
    await apiFetch<{ ok: boolean }>("/api/logout", {
        method: "POST",
        body: refreshToken ? { refresh_token: refreshToken } : {},
    });
}
