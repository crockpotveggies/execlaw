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

export interface NodeDef {
    id: string;
    kind: NodeKind;
    /** Kind-specific config (Rhai expr, AskAgent prompt + exit_tools, etc.). */
    config: unknown;
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
