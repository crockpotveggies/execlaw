// Strongly-typed client for the M4a Automations admin API. Backs the
// `/automations` landing page + detail page + runs drawer.
//
// Kept in its own file (not endpoints.ts) so the automation surface
// stays cohesive — adds 12 endpoints and the related DTOs, which
// would crowd the omnibus client.

import { ApiError, apiFetch } from "./client";

// ---- Wire shapes (mirror Rust DTOs in server/automations_admin.rs) ----

export type BusEventKind =
    | "webhook.received"
    | "socket.message"
    | "plugin.emit"
    | "routine.fired"
    | "other";

export type NodeKind =
    | "Filter"
    | "Transform"
    | "Branch"
    | "Terminal"
    | "AskAgent"
    // Reserved (server-side validator rejects with NotYetImplemented):
    | "CallPlugin"
    | "AppendToChat"
    | "HttpFetch"
    | "Notify"
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

export interface SampleEventBody {
    kind: BusEventKind;
    source: string;
    payload: unknown;
}

export interface TestRunRequest {
    event_id?: string;
    sample_event?: SampleEventBody;
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

/** Friendly label for the trigger-kind dropdown. */
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
        case "other":
            return "Other";
    }
}

export function formatPercent(v: number | null): string {
    if (v === null) return "—";
    return `${(v * 100).toFixed(1)}%`;
}

// Re-export ApiError so consumers don't have to dual-import.
export { ApiError };
