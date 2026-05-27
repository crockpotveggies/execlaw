// Strongly-typed client for the M4a Automations admin API. Backs the
// `/automations` landing page + detail page + runs drawer.
//
// Kept in its own file (not endpoints.ts) so the automation surface
// stays cohesive — adds 12 endpoints and the related DTOs, which
// would crowd the omnibus client.

import { ApiError, apiFetch } from "./client";

// ---- Wire shapes (mirror Rust DTOs in server/automations_admin.rs) ----

/**
 * Bus-event kind. Open alphabet — any dotted string is valid; plugins
 * declare their own kinds via the registry (see RegisteredEventKind).
 * The canonical list at runtime comes from
 * `/api/admin/automations/registered-events`.
 *
 * The constants below are the well-known kinds that ship with core or
 * are referenced in legacy code paths; the type stays `string` so
 * plugin-defined kinds work too.
 */
export type BusEventKind = string;

export const KNOWN_BUS_EVENT_KINDS = [
    "webhook.received",
    "socket.message",
    "plugin.emit",
    "routine.fired",
    "web.prompt.submitted",
    "other",
] as const;

export type NodeKind =
    | "Filter"
    | "Transform"
    | "Branch"
    | "Terminal"
    // Phase A of the Flows middleware redesign (2026-05-22):
    // rewrites the user-facing prompt the chat turn driver sees.
    // Config: { expr: "<Rhai expression returning a string>" }
    | "RewritePrompt"
    // Phase B mutators (2026-05-22): pre-turn middleware that
    // mutates the chat handler's effective inputs before the turn
    // driver runs. SetSkills/SetTools/AddAttachment/AddMemory are
    // additive lists; SetTrust is a scalar override.
    | "SetSkills"     // { skills: ["name", ...] }
    | "SetTools"      // { tools: ["name", ...] }
    | "SetTrust"      // { trust: "controller"|"known_high"|... }
    | "AddAttachment" // { attachment_ids: ["id", ...] }
    | "AddMemory"     // { text: "string" }
    | "AskAgent"
    | "Notify"
    | "CallPlugin"
    // M6 — emits a reply through the ReplyRouter using envelope.origin
    | "SendReply"
    // Reserved (server-side validator rejects with NotYetImplemented):
    | "AppendToChat"
    | "HttpFetch"
    | "AwaitApproval"
    | "CallAutomation"
    | "Parallel"
    | "Join";

export interface TriggerDef {
    kind: BusEventKind;
    /** Optional Rhai predicate over `{ event: ... }`. */
    when?: string | null;
}

export interface NodePosition {
    x: number;
    y: number;
}

export interface ExitToolDef {
    name: string;
    description: string;
    args_schema?: unknown;
}

export interface NodeDef {
    id: string;
    kind: NodeKind;
    /** Kind-specific config (Rhai expr, AskAgent prompt + exit_tools, etc.). */
    config: unknown;
    /** Persisted canvas coordinates. Optional — server returns
     *  undefined for nodes saved before canvas-editor v2; the SPA
     *  falls back to BFS layout for those. */
    position?: NodePosition | null;
}

export interface EdgeDef {
    from: string;
    to: string;
    when?: string | null;
}

export interface AutomationDef {
    trigger: TriggerDef;
    nodes: NodeDef[];
    edges: EdgeDef[];
}

export interface AutomationView {
    id: string;
    name: string;
    enabled: boolean;
    definition: AutomationDef;
    created_at: number;
    updated_at: number;
    /** M6 — `"operator"` | `"core"` | `"plugin:<id>"`. */
    source?: string;
    /** M6 — `true` when the operator has edited a non-operator row. */
    operator_modified?: boolean;
    /** M6 — convenience: `true` for core- + plugin-shipped defaults.
     *  When true the SPA hides the delete button (server-side
     *  delete returns 403 with code `automation_is_default`). */
    is_default?: boolean;
}

export interface UpsertAutomationBody {
    name: string;
    enabled: boolean;
    definition: AutomationDef;
}

export type AutomationRunStatus =
    | "pending"
    | "running"
    | "success"
    | "failed"
    | "skipped";

export interface StepTrace {
    node_id: string;
    input: unknown;
    output: unknown;
    ms: number;
    error?: string | null;
}

export interface AutomationRunView {
    id: string;
    automation_id: string;
    event_id: string;
    status: AutomationRunStatus;
    step_traces: StepTrace[];
    started_at: number;
    finished_at: number | null;
}

export interface AutomationMetrics {
    /** Count of enabled automations. */
    active_count: number;
    /** Count of runs in the last 24h (any status). */
    runs_24h: number;
    /** Fraction in [0,1], or `null` when there are no runs in the window. */
    success_rate_24h: number | null;
    /** Distinct `(kind, source)` pairs in the last 24h that no enabled automation consumes. */
    untriaged_kinds_24h: number;
}

export interface SuggestionView {
    id: string;
    kind: BusEventKind;
    source: string;
    event_count: number;
    sample_event_ids: string[];
    suggested_name: string;
    created_at: number;
    updated_at: number;
    /** M5: present when an agent-drafting path populated a seed
     *  graph the editor can pre-fill. `null` for plain pattern-
     *  detected suggestions. */
    draft_definition?: AutomationDef | null;
}

// ---- Endpoints ----

const BASE = "/api/admin/automations";

export async function listAutomations(
    tokenAccessor: () => string | null,
): Promise<AutomationView[]> {
    return apiFetch<AutomationView[]>(BASE, {}, tokenAccessor);
}

export async function getAutomation(
    id: string,
    tokenAccessor: () => string | null,
): Promise<AutomationView> {
    return apiFetch<AutomationView>(`${BASE}/${encodeURIComponent(id)}`, {}, tokenAccessor);
}

export async function createAutomation(
    body: UpsertAutomationBody,
    tokenAccessor: () => string | null,
): Promise<AutomationView> {
    return apiFetch<AutomationView>(
        BASE,
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function updateAutomation(
    id: string,
    body: UpsertAutomationBody,
    tokenAccessor: () => string | null,
): Promise<AutomationView> {
    return apiFetch<AutomationView>(
        `${BASE}/${encodeURIComponent(id)}`,
        { method: "PUT", body },
        tokenAccessor,
    );
}

export async function deleteAutomation(
    id: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `${BASE}/${encodeURIComponent(id)}`,
        { method: "DELETE" },
        tokenAccessor,
    );
}

export async function setAutomationEnabled(
    id: string,
    enabled: boolean,
    tokenAccessor: () => string | null,
): Promise<void> {
    const suffix = enabled ? "enable" : "disable";
    await apiFetch<unknown>(
        `${BASE}/${encodeURIComponent(id)}/${suffix}`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

export async function listAutomationRuns(
    id: string,
    tokenAccessor: () => string | null,
): Promise<AutomationRunView[]> {
    return apiFetch<AutomationRunView[]>(
        `${BASE}/${encodeURIComponent(id)}/runs`,
        {},
        tokenAccessor,
    );
}

export async function getAutomationMetrics(
    tokenAccessor: () => string | null,
): Promise<AutomationMetrics> {
    return apiFetch<AutomationMetrics>(`${BASE}/metrics`, {}, tokenAccessor);
}

export async function listSuggestions(
    tokenAccessor: () => string | null,
): Promise<SuggestionView[]> {
    return apiFetch<SuggestionView[]>(`${BASE}/suggestions`, {}, tokenAccessor);
}

export async function dismissSuggestion(
    id: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `${BASE}/suggestions/${encodeURIComponent(id)}/dismiss`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

export async function getSuggestion(
    id: string,
    tokenAccessor: () => string | null,
): Promise<SuggestionView> {
    return apiFetch<SuggestionView>(
        `${BASE}/suggestions/${encodeURIComponent(id)}`,
        {},
        tokenAccessor,
    );
}

export async function actionSuggestion(
    id: string,
    tokenAccessor: () => string | null,
): Promise<void> {
    await apiFetch<unknown>(
        `${BASE}/suggestions/${encodeURIComponent(id)}/action`,
        { method: "POST", body: {} },
        tokenAccessor,
    );
}

// ---- Test-run + sample payloads (M4c) ----

export type ExecOutcome = "success" | "skipped" | "failed";

export interface DryRunResult {
    outcome: ExecOutcome;
    step_traces: StepTrace[];
}

export interface RecentBusEvent {
    id: string;
    kind: BusEventKind;
    source: string;
    received_at: number;
    payload: unknown;
}

/**
 * Origin reference — mirrors the Rust `OriginRef` enum's `serde(tag =
 * "kind")` shape. Used in the test-run drawer to test envelope-gated
 * trigger filters without having to wait for a real event of the
 * matching kind.
 */
export type OriginRef =
    | { kind: "web_socket_session"; session_id: string }
    | {
          kind: "plugin_channel";
          plugin_id: string;
          channel_ref: unknown;
          expires_at_ms?: number | null;
      }
    | { kind: "chat_append"; thread_id: string }
    | { kind: "alert" }
    | { kind: "none" };

/** Mirrors the Rust `SenderIdentity` enum. */
export type SenderIdentity =
    | { kind: "principal"; id: string; trust: TrustClass }
    | {
          kind: "external";
          plugin_id: string;
          handle: string;
          trust: TrustClass;
      }
    | { kind: "system" };

/** Trust class — keep in lockstep with Rust `core::trust::TrustClass`.
 *  Lower-cased per `#[serde(rename_all = "snake_case")]`. */
export type TrustClass =
    | "controller"
    | "known_high"
    | "known_limited"
    | "cold_contact"
    | "blocked";

export interface EventEnvelope {
    origin: OriginRef;
    identity: SenderIdentity;
    correlation_id: string;
    parent_event_id?: string | null;
}

export interface SampleEventBody {
    kind: BusEventKind;
    source: string;
    payload: unknown;
    /** Optional envelope. When omitted the server defaults to
     *  `EventEnvelope::system_internal()`. */
    envelope?: EventEnvelope;
}

export interface TestRunRequest {
    event_id?: string;
    sample_event?: SampleEventBody;
    /** Caller-supplied run id. When set, FlowChannelHub publishes
     *  under this id so SSE subscribers can correlate. */
    client_run_id?: string;
}

export async function testRunAutomation(
    id: string,
    body: TestRunRequest,
    tokenAccessor: () => string | null,
): Promise<DryRunResult> {
    return apiFetch<DryRunResult>(
        `${BASE}/${encodeURIComponent(id)}/test-run`,
        { method: "POST", body },
        tokenAccessor,
    );
}

export async function listRecentBusEvents(
    kind: BusEventKind,
    limit: number,
    tokenAccessor: () => string | null,
): Promise<RecentBusEvent[]> {
    const q = new URLSearchParams({ kind, limit: String(limit) });
    return apiFetch<RecentBusEvent[]>(
        `${BASE}/recent-events?${q.toString()}`,
        {},
        tokenAccessor,
    );
}

// ---- M6 registry inspection ----

export interface RegisteredEventKind {
    kind: string;
    source: string;
    description: string;
    payload_schema?: unknown;
    expects_reply: boolean;
    default_origin_kind: string;
}

export interface RegisteredReplyHandler {
    name: string;
    plugin_id: string;
    description: string;
    supports_streaming: boolean;
    supports_attachments: boolean;
    supports_inline_chart: boolean;
    supports_table: boolean;
    supports_card: boolean;
    supports_markdown: boolean;
    max_attachment_size_bytes?: number;
    max_attachments_per_message?: number;
    max_text_length?: number;
    allowed_mime_prefixes?: string[];
}

export interface DefaultFlowSummary {
    id: string;
    name: string;
    enabled: boolean;
    source: string;
    source_version?: string;
    operator_modified: boolean;
}

export async function listRegisteredEvents(
    tokenAccessor: () => string | null,
): Promise<RegisteredEventKind[]> {
    return apiFetch<RegisteredEventKind[]>(
        `${BASE}/registered-events`,
        {},
        tokenAccessor,
    );
}

export async function listRegisteredReplyHandlers(
    tokenAccessor: () => string | null,
): Promise<RegisteredReplyHandler[]> {
    return apiFetch<RegisteredReplyHandler[]>(
        `${BASE}/registered-reply-handlers`,
        {},
        tokenAccessor,
    );
}

export async function listDefaultFlows(
    tokenAccessor: () => string | null,
): Promise<DefaultFlowSummary[]> {
    return apiFetch<DefaultFlowSummary[]>(
        `${BASE}/default-flows`,
        {},
        tokenAccessor,
    );
}

/**
 * Slice G (2026-05-26) — operator-facing branch-author hints
 * collected from every installed plugin's `[[branch_suggestions]]`
 * section. The Branch / edge.when / trigger.when pickers fetch this
 * once per flow-edit session and filter client-side by the active
 * flow's trigger.kind.
 *
 * Shape mirrors the Rust DTO `BranchSuggestionDto` 1:1.
 */
export interface BranchSuggestion {
    /** Trigger / event kind this suggestion is meaningful for. */
    event_kind: string;
    /**
     * Optional secondary gate — the SPA hides the suggestion when
     * the flow's existing `trigger.when` doesn't contain this
     * expression as a substring. Loose heuristic, not a runtime
     * check; keeps slack-specific suggestions out of whatsapp-
     * narrowed flows and vice versa.
     */
    when_active?: string;
    /** Operator-facing dropdown label. */
    display_name: string;
    /** One-line hint under the label. */
    description: string;
    /** Rhai expression with `{placeholder}` chips. */
    template: string;
    /** Default value per placeholder; key matches the `{name}` in template. */
    defaults: Record<string, string>;
    /** Plugin that contributed this suggestion (badge in the dropdown). */
    source_plugin_id: string;
    /** Plugin version (helps with stale-suggestion debugging). */
    source_plugin_version: string;
}

export async function listBranchSuggestions(
    tokenAccessor: () => string | null,
): Promise<BranchSuggestion[]> {
    return apiFetch<BranchSuggestion[]>(
        `${BASE}/branch-suggestions`,
        {},
        tokenAccessor,
    );
}

/**
 * Resolve a branch-suggestion template to a concrete Rhai expression
 * by substituting `{placeholder}` chips with the supplied values
 * (falling back to the suggestion's defaults for keys the caller
 * didn't override). Pure helper — no network. Use from picker forms
 * so the rendered expression stays consistent across the three
 * surfaces (Branch, edge.when, trigger.when).
 */
export function renderBranchSuggestion(
    suggestion: BranchSuggestion,
    overrides: Record<string, string> = {},
): string {
    let out = suggestion.template;
    const merged = { ...suggestion.defaults, ...overrides };
    for (const [key, value] of Object.entries(merged)) {
        // Match `{key}` literally; Rhai expressions never contain
        // braces in non-placeholder positions for our templates,
        // so a global replace is safe.
        out = out.split(`{${key}}`).join(value);
    }
    return out;
}

/**
 * Filter a suggestion list to those relevant to the current flow.
 * Match rule:
 *   1. `event_kind` must equal the flow's trigger kind.
 *   2. If `when_active` is set, the flow's current trigger.when
 *      (or other upstream context the caller threads in) must
 *      contain `when_active` as a substring. Loose match — the
 *      goal is "operator narrowed to slack already, only show
 *      slack-shaped suggestions," not exact-equal.
 */
export function filterBranchSuggestions(
    suggestions: BranchSuggestion[],
    activeEventKind: string,
    activeContext: string,
): BranchSuggestion[] {
    return suggestions.filter((s) => {
        if (s.event_kind !== activeEventKind) {
            return false;
        }
        if (s.when_active && !activeContext.includes(s.when_active)) {
            return false;
        }
        return true;
    });
}

/** POST /api/web/prompt — M6 web-prompt entrypoint (shadow mode). */
export interface SubmitPromptRequest {
    text: string;
    session_id: string;
    conversation_id?: string;
    attachment_ids?: string[];
}

export interface SubmitPromptResponse {
    event_id: string;
}

export async function submitWebPrompt(
    body: SubmitPromptRequest,
    tokenAccessor: () => string | null,
): Promise<SubmitPromptResponse> {
    return apiFetch<SubmitPromptResponse>(
        "/api/web/prompt",
        {
            method: "POST",
            body,
        },
        tokenAccessor,
    );
}

// ---- Helpers for templated defaults the UI uses ----

/** Minimal valid graph: trigger → Terminal. Used as the seed when the
 *  operator creates a fresh automation without a suggestion template. */
export function emptyAutomationDef(kind: BusEventKind = "webhook.received"): AutomationDef {
    return {
        trigger: { kind, when: null },
        nodes: [
            { id: "end", kind: "Terminal", config: {} },
        ],
        edges: [
            { from: "trigger", to: "end", when: null },
        ],
    };
}

/** Friendly label for a trigger-kind shown in dropdowns and badges.
 *
 *  With BusEventKind open to plugin-defined kinds, we humanize the
 *  dotted form: `whatsapp.message.received` -> "WhatsApp message
 *  received". The registry's `description` field (when available) is a
 *  better source, but callers without registry context fall through to
 *  this. */
export function kindLabel(k: BusEventKind): string {
    switch (k) {
        case "webhook.received":
            return "Webhook received";
        case "socket.message":
            return "Socket message";
        case "plugin.emit":
            return "Plugin emit";
        case "routine.fired":
            return "Routine fired";
        case "web.prompt.submitted":
            return "Web prompt submitted";
        case "other":
            return "Other";
    }
    // Humanize plugin-declared kinds:
    //   "whatsapp.message.received" -> "Whatsapp message received"
    //   "calendar.event.starting_soon" -> "Calendar event starting soon"
    if (!k) return "(unknown)";
    const words = k.replace(/[._]/g, " ").trim();
    if (words.length === 0) return k;
    return words.charAt(0).toUpperCase() + words.slice(1).toLowerCase();
}

export function formatPercent(v: number | null): string {
    if (v === null) return "—";
    return `${(v * 100).toFixed(1)}%`;
}

// Re-export ApiError so consumers don't have to dual-import.
export { ApiError };
