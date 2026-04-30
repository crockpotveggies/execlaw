// Running-jobs badge — sits above the composer in any conversation
// with active research cards (C6).
//
// Reads from the existing per-conversation card store rather than
// polling a server endpoint: every research card the runner emits
// is already in the store via the WS event bus, so the badge stays
// live without an extra round-trip. Clicks navigate to /research
// (deep-linked to the specific job when there's only one).
//
// 2026-04-29.

import { Link } from "react-router-dom";
import { useCardsForConversation } from "../cards/cardStore";
import { TERMINAL_STATES, type Card } from "../cards/types";

interface Props {
    conversationId: string;
}

export function RunningJobsBadge({ conversationId }: Props) {
    const cards = useCardsForConversation(conversationId);
    const activeResearch = cards.filter(
        (c) => c.kind === "research" && !TERMINAL_STATES.has(c.state),
    );
    if (activeResearch.length === 0) return null;
    return (
        <div
            className="execlaw-running-jobs-badge"
            data-testid="running-jobs-badge"
            data-count={activeResearch.length}
        >
            <i className="bi bi-binoculars me-2" aria-hidden />
            {activeResearch.length === 1
                ? renderSingle(activeResearch[0])
                : renderMultiple(activeResearch.length)}
        </div>
    );
}

function renderSingle(card: Card) {
    const phase = card.phase ?? "Running";
    return (
        <>
            <span className="me-2">
                Researching · {phase}
                {card.progress !== null && (
                    <span className="ms-2 execlaw-muted small">
                        {Math.round(card.progress * 100)}%
                    </span>
                )}
            </span>
            <Link
                to={`/research/${encodeURIComponent(extractJobId(card))}`}
                data-testid="running-jobs-badge-link"
            >
                Open
            </Link>
        </>
    );
}

function renderMultiple(n: number) {
    return (
        <>
            <span className="me-2">{n} research jobs running</span>
            <Link to="/research" data-testid="running-jobs-badge-link">
                Open
            </Link>
        </>
    );
}

/// Card details_json carries `job_id` per the runner's contract.
/// Falls back to the card_id when the key is missing — defensive
/// against a future renderer-side schema change.
function extractJobId(card: Card): string {
    if (
        card.details &&
        typeof card.details === "object" &&
        card.details !== null &&
        "job_id" in card.details
    ) {
        const v = (card.details as { job_id?: unknown }).job_id;
        if (typeof v === "string") return v;
    }
    return card.card_id;
}
