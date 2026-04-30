// TypeScript mirror of `execlaw_core::cards::*`. Keep these
// signatures in lockstep with the Rust source — the wire payloads
// are MessagePack-encoded by the server and decoded into JSON for
// the WS event stream, so any drift here surfaces as a runtime
// projection failure rather than a compile error.
//
// 2026-04-29.

export type CardKind =
    | "long_running_task"
    | "research"
    | "shell_session"
    | "file_pipeline";

export type CardState =
    | "Pending"
    | "Running"
    | "Paused"
    | "Completed"
    | "Failed"
    | "Cancelled";

export const TERMINAL_STATES: ReadonlySet<CardState> = new Set([
    "Completed",
    "Failed",
    "Cancelled",
]);

export type CardAction =
    | { kind: "Cancel" }
    | { kind: "Pause" }
    | { kind: "Resume" }
    | { kind: "OpenDetail"; href: string };

export interface CardOpenedPayload {
    card_id: string;
    kind: CardKind;
    title: string;
    summary: string;
    state?: CardState;
    details?: unknown;
    actions?: CardAction[];
}

export interface CardProgressedPayload {
    card_id: string;
    state?: CardState;
    progress?: number;
    phase?: string;
    details?: unknown;
    actions?: CardAction[];
    summary?: string;
}

export interface CardClosedPayload {
    card_id: string;
    state: CardState;
    summary: string;
    details?: unknown;
    attachment_id?: string;
    error?: string;
}

/// The live shape the SPA renders. Composed by `applyEvent` from a
/// `CardOpenedPayload` plus zero or more `CardProgressedPayload` /
/// `CardClosedPayload` events.
export interface Card {
    card_id: string;
    conversation_id: string;
    kind: CardKind;
    state: CardState;
    title: string;
    summary: string;
    progress: number | null;
    phase: string | null;
    details: unknown;
    actions: CardAction[];
    attachment_id: string | null;
    error: string | null;
    opened_at: number; // unix-seconds
    updated_at: number;
}

/// Discriminated union surfaced through the WS event stream after
/// the server decodes the MessagePack payload to JSON.
export type CardEvent =
    | { kind: "card.opened"; payload: CardOpenedPayload; committed_at: number }
    | { kind: "card.progressed"; payload: CardProgressedPayload; committed_at: number }
    | { kind: "card.closed"; payload: CardClosedPayload; committed_at: number };

/// Whether to render the live progress UI vs. the static
/// "completed" summary. Kind-specific renderers can use this to
/// drop progress bars / spinners in favour of a final-result view.
export function isTerminal(state: CardState): boolean {
    return TERMINAL_STATES.has(state);
}
