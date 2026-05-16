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
    | "file_pipeline"
    | "attachment"
    // 2026-05-16 — chart card produced by the native `chart.render`
    // built-in tool. Mirrors the `CardKind::Chart` Rust variant
    // (added in `crates/core/src/cards.rs` as part of the
    // chart-render → cards-path migration). Renderer registered at
    // `web/src/cards/ChartCard.tsx`.
    | "chart";

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
    /// 2026-05-03 — `state_events.seq` of the `CardOpened` event,
    /// used by `MessageStream` to place the card inline in the
    /// chat-thread timeline (instead of synthesizing a sortKey from
    /// `opened_at` that always pinned cards to the bottom).
    /// Optional because legacy bus events / fallback projections
    /// may not carry it; the renderer falls back to the synthetic
    /// sortKey when this is null.
    event_seq: number | null;
}

/// Discriminated union surfaced through the WS event stream after
/// the server decodes the MessagePack payload to JSON.
export type CardEvent =
    | {
          kind: "card.opened";
          payload: CardOpenedPayload;
          committed_at: number;
          event_seq?: number | null;
      }
    | {
          kind: "card.progressed";
          payload: CardProgressedPayload;
          committed_at: number;
          event_seq?: number | null;
      }
    | {
          kind: "card.closed";
          payload: CardClosedPayload;
          committed_at: number;
          event_seq?: number | null;
      };

/// Whether to render the live progress UI vs. the static
/// "completed" summary. Kind-specific renderers can use this to
/// drop progress bars / spinners in favour of a final-result view.
export function isTerminal(state: CardState): boolean {
    return TERMINAL_STATES.has(state);
}
