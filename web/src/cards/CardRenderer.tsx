// Card renderer registry + the `LongRunningTask` generic renderer.
//
// Per-kind components register here at module-load so the chat-pane
// can dispatch a card to the right renderer without knowing which
// kinds the rest of the SPA defines. Future kinds (`Research`,
// `ShellSession`, `FilePipeline`) ship their own renderer file and
// import-side-effect-register here.
//
// 2026-04-29.

import type { ComponentType } from "react";
import type { Card, CardKind } from "./types";

/// Props every per-kind renderer accepts. Keep narrow — kind-specific
/// data lives inside `card.details`, which the renderer interprets
/// according to its own contract.
export interface CardRendererProps {
    card: Card;
    /// Operator clicked a `CardAction`. Wires through to a server
    /// endpoint in the production chat pane; the registry doesn't
    /// care how. Tests can stub.
    onAction?: (actionId: string) => void;
}

const REGISTRY = new Map<CardKind, ComponentType<CardRendererProps>>();

export function registerCardRenderer(
    kind: CardKind,
    component: ComponentType<CardRendererProps>,
): void {
    REGISTRY.set(kind, component);
}

export function getCardRenderer(
    kind: CardKind,
): ComponentType<CardRendererProps> {
    // Unknown kinds fall back to the generic LongRunningTask renderer
    // so a forwards-compat plugin emitting a future `kind` still
    // displays a usable progress card.
    return REGISTRY.get(kind) ?? LongRunningTaskCard;
}

/// Generic catch-all card renderer. Shows title, phase, progress
/// bar, summary, and actions. Used as the fallback for unknown
/// `kind` values and as the default for the explicit
/// `LongRunningTask` kind.
export function LongRunningTaskCard({ card, onAction }: CardRendererProps) {
    const pct =
        card.progress !== null ? Math.round(card.progress * 100) : null;
    const showProgress = pct !== null && card.state !== "Completed";
    return (
        <div
            className="execlaw-card-task"
            data-testid="card-long-running-task"
            data-card-id={card.card_id}
            data-state={card.state}
        >
            <div className="execlaw-card-task__head">
                <span className="execlaw-card-task__title">{card.title}</span>
                <span
                    className="execlaw-card-task__state"
                    data-testid="card-state"
                >
                    {card.state}
                </span>
            </div>
            {card.phase && (
                <div
                    className="execlaw-card-task__phase"
                    data-testid="card-phase"
                >
                    {card.phase}
                </div>
            )}
            {showProgress && (
                <div
                    className="execlaw-card-task__progress"
                    role="progressbar"
                    aria-valuenow={pct ?? 0}
                    aria-valuemin={0}
                    aria-valuemax={100}
                    data-testid="card-progress"
                >
                    <div
                        className="execlaw-card-task__progress-bar"
                        style={{ width: `${pct}%` }}
                    />
                    <span className="execlaw-card-task__progress-label">
                        {pct}%
                    </span>
                </div>
            )}
            <div className="execlaw-card-task__summary">{card.summary}</div>
            {card.error && (
                <div
                    className="execlaw-card-task__error"
                    data-testid="card-error"
                >
                    {card.error}
                </div>
            )}
            {card.actions.length > 0 && onAction && (
                <div className="execlaw-card-task__actions">
                    {card.actions.map((a, idx) => {
                        const id =
                            a.kind === "Cancel"
                                ? "cancel"
                                : a.kind === "Pause"
                                  ? "pause"
                                  : a.kind === "Resume"
                                    ? "resume"
                                    : "open_detail";
                        return (
                            <button
                                key={`${id}-${idx}`}
                                type="button"
                                className="execlaw-card-task__action"
                                onClick={() => onAction(id)}
                                data-testid={`card-action-${id}`}
                            >
                                {a.kind}
                            </button>
                        );
                    })}
                </div>
            )}
        </div>
    );
}

// Auto-register the generic renderer for the explicit kind too.
registerCardRenderer("long_running_task", LongRunningTaskCard);
