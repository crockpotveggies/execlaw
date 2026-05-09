// Card projection — TypeScript mirror of `Card::apply` /
// `Card::from_opened` in `crates/core/src/cards.rs`.
//
// Pure-functional shape: take a card (or `null` when applying the
// initial Opened event) and an event, return the new card. The
// chat-state hook stores `Map<card_id, Card>` and applies events
// arriving over the WS to that map.
//
// 2026-04-29.

import type {
    Card,
    CardEvent,
    CardOpenedPayload,
} from "./types";

const clamp01 = (n: number): number => Math.min(1, Math.max(0, n));

/// Build a fresh card from a `CardOpened` event. Returns `null` for
/// any other event kind so the caller can skip out-of-band events
/// (Progressed/Closed before Opened).
export function fromOpened(
    conversationId: string,
    ev: CardEvent,
): Card | null {
    if (ev.kind !== "card.opened") return null;
    const p = ev.payload;
    return {
        card_id: p.card_id,
        conversation_id: conversationId,
        kind: p.kind,
        state: p.state ?? "Pending",
        title: p.title,
        summary: p.summary,
        progress: null,
        phase: null,
        details: p.details ?? {},
        actions: p.actions ?? [],
        attachment_id: null,
        error: null,
        opened_at: ev.committed_at,
        updated_at: ev.committed_at,
        // 2026-05-03 — `state_events.seq` of the CardOpened event.
        // When present (server >= the same date), MessageStream uses
        // it as the sortKey so cards land at their true position in
        // the chat-thread timeline. `null` for legacy WS payloads
        // and the renderer falls back to the synthetic sortKey.
        event_seq: ev.event_seq ?? null,
    };
}

/// Apply an event to a card. Returns the same reference when the
/// event's `card_id` doesn't match (defensive against the projection
/// layer crossing wires) and a new merged card otherwise. Pure —
/// `card` is never mutated; React reference equality stays sane.
export function applyEvent(card: Card, ev: CardEvent): Card {
    const targetId =
        ev.kind === "card.opened"
            ? ev.payload.card_id
            : ev.kind === "card.progressed"
              ? ev.payload.card_id
              : ev.payload.card_id;
    if (targetId !== card.card_id) return card;

    if (ev.kind === "card.opened") {
        // CardOpened on an already-open card — happens on
        // clarification resume (the runner emits a fresh Open with
        // the same card_id when claim_next_pending re-claims a
        // Pending row). Replace title/summary/state/details/actions
        // AND clear the stale `phase` (which would otherwise stay
        // on "AwaitingInput" after the resume), AND bump event_seq
        // so the card pops to its new position in the timeline.
        const p = ev.payload as CardOpenedPayload;
        return {
            ...card,
            kind: p.kind,
            state: p.state ?? card.state,
            title: p.title,
            summary: p.summary,
            // Reset phase — the runner emits an explicit
            // CardProgressed{phase: "Planning"} immediately after
            // this on the resume path; clearing here avoids a
            // single-frame flash of the stale label.
            phase: null,
            details: p.details ?? card.details,
            actions: p.actions ?? card.actions,
            // Surface-seq bump — this card was just (re-)opened,
            // so it pops to the bottom of the inline timeline.
            event_seq: ev.event_seq ?? card.event_seq,
        };
    }

    if (ev.kind === "card.progressed") {
        const p = ev.payload;
        return {
            ...card,
            state: p.state ?? card.state,
            progress: p.progress !== undefined ? clamp01(p.progress) : card.progress,
            phase: p.phase !== undefined ? p.phase : card.phase,
            details: p.details !== undefined ? p.details : card.details,
            actions: p.actions ?? card.actions,
            summary: p.summary !== undefined ? p.summary : card.summary,
            updated_at: ev.committed_at,
        };
    }

    // ev.kind === "card.closed" — terminal state. One-time pop on
    // the close event so the deliverable surfaces inline with the
    // latest activity, then frozen (further messages naturally
    // appear after this card without re-popping it).
    const p = ev.payload;
    // 2026-05-04 — derive the post-close phase from
    // details.phase when the runner stamped it (today: "Complete"
    // for the deep-research path), else null. Without this, the
    // last CardProgressed phase ("Synthesizing", "Gathering" etc.)
    // stays rendered on the card even after the Completed badge
    // shows — visible to the operator as a stuck "Synthesizing"
    // label below a "Completed" badge.
    const nextDetails = p.details !== undefined ? p.details : card.details;
    const phaseFromDetails =
        nextDetails &&
        typeof nextDetails === "object" &&
        "phase" in (nextDetails as Record<string, unknown>) &&
        typeof (nextDetails as Record<string, unknown>).phase === "string"
            ? ((nextDetails as Record<string, unknown>).phase as string)
            : null;
    return {
        ...card,
        state: p.state,
        summary: p.summary,
        details: nextDetails,
        phase: phaseFromDetails,
        attachment_id:
            p.attachment_id !== undefined ? p.attachment_id : card.attachment_id,
        error: p.error !== undefined ? p.error : card.error,
        updated_at: ev.committed_at,
        // Surface-seq bump on close so the completed card pops to
        // the bottom of the timeline once. CardProgressed events
        // (intermediate ticks) deliberately don't bump — only the
        // milestone transitions do.
        event_seq: ev.event_seq ?? card.event_seq,
    };
}

/// Replay a sequence of events into a final card. Used by tests + by
/// the SPA's initial chat-history bootstrap (which gets a batch of
/// historic events before live updates start streaming).
export function projectFromEvents(
    conversationId: string,
    events: CardEvent[],
): Map<string, Card> {
    const cards = new Map<string, Card>();
    for (const ev of events) {
        if (ev.kind === "card.opened") {
            const fresh = fromOpened(conversationId, ev);
            if (fresh) {
                // If a prior card with the same id exists, merge the
                // re-key into it rather than discarding history.
                const existing = cards.get(fresh.card_id);
                if (existing) {
                    cards.set(fresh.card_id, applyEvent(existing, ev));
                } else {
                    cards.set(fresh.card_id, fresh);
                }
            }
        } else {
            const id =
                ev.kind === "card.progressed"
                    ? ev.payload.card_id
                    : ev.payload.card_id;
            const existing = cards.get(id);
            if (existing) {
                cards.set(id, applyEvent(existing, ev));
            }
            // Out-of-band Progressed/Closed (no prior Open): skip.
        }
    }
    return cards;
}
