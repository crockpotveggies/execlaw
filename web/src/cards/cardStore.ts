// Per-conversation card store — projects WS card.* events into a
// `Map<conversation_id, Map<card_id, Card>>`. Mirrors the Rust
// projection in `crates/core/src/cards.rs` via `applyEvent` from
// `./projection.ts`.
//
// The chat-pane MessageStream reads cards for the active
// conversation and renders them inline-chronological with messages
// (sort key: `Card.opened_at`). New cards arriving after first
// render trigger a re-render via `useSyncExternalStore`.
//
// 2026-04-29.

import { useSyncExternalStore } from "react";
import type { Card, CardEvent } from "./types";
import { applyEvent, fromOpened } from "./projection";

interface CardStoreState {
    /// Outer key: conversation_id. Inner: card_id → Card.
    byConversation: Record<string, Record<string, Card>>;
}

type Listener = () => void;

const listeners = new Set<Listener>();
let state: CardStoreState = { byConversation: {} };

function emit() {
    for (const l of listeners) l();
}

function setState(next: CardStoreState) {
    if (next === state) return;
    state = next;
    emit();
}

export function getCardStoreState(): CardStoreState {
    return state;
}

export function subscribeCardStore(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
}

/// Hook returning every card on the given conversation, sorted by
/// `opened_at` ascending so the chat pane can interleave them with
/// messages by sequence.
export function useCardsForConversation(conversationId: string | null): Card[] {
    return useSyncExternalStore(
        subscribeCardStore,
        () => {
            if (!conversationId) return EMPTY_CARDS;
            const byId = state.byConversation[conversationId];
            if (!byId) return EMPTY_CARDS;
            return memoizedSorted(conversationId, byId);
        },
        () => EMPTY_CARDS,
    );
}

const EMPTY_CARDS: Card[] = [];

// `useSyncExternalStore` is allergic to a fresh array on every read
// (it triggers an infinite render loop). Memoise the sorted result
// keyed by the underlying object identity — when applyCardEvent
// returns the same `byId` ref (event for a different card_id, or
// no-op), this returns the cached array.
const sortedCache = new WeakMap<Record<string, Card>, Card[]>();
function memoizedSorted(
    _conv: string,
    byId: Record<string, Card>,
): Card[] {
    const cached = sortedCache.get(byId);
    if (cached) return cached;
    const arr = Object.values(byId).sort(
        (a, b) => a.opened_at - b.opened_at || a.card_id.localeCompare(b.card_id),
    );
    sortedCache.set(byId, arr);
    return arr;
}

export function applyCardEvent(
    conversationId: string,
    ev: CardEvent,
): void {
    const conversations = state.byConversation;
    const prior = conversations[conversationId] ?? {};
    let nextForConv: Record<string, Card> | null = null;

    if (ev.kind === "card.opened") {
        const fresh = fromOpened(conversationId, ev);
        if (!fresh) return;
        const existing = prior[fresh.card_id];
        const merged = existing ? applyEvent(existing, ev) : fresh;
        if (existing && merged === existing) return;
        nextForConv = { ...prior, [fresh.card_id]: merged };
    } else {
        const cardId = ev.payload.card_id;
        const existing = prior[cardId];
        if (!existing) {
            // Out-of-band Progressed/Closed (no prior Open): skip.
            // The runner's commit-then-broadcast contract makes this
            // rare — a WS-side miss can't lose the Open without
            // also losing the Progressed/Closed (same conversation,
            // same broadcast channel) — but skip defensively.
            return;
        }
        const merged = applyEvent(existing, ev);
        if (merged === existing) return;
        nextForConv = { ...prior, [cardId]: merged };
    }

    setState({
        byConversation: {
            ...conversations,
            [conversationId]: nextForConv!,
        },
    });
}

/// Replace the card set for one conversation in one shot.
///
/// 2026-05-04: used by the chat-pane on thread load to seed the
/// store from the server's `GET /api/chats/{cid}/cards`
/// projection. Without this, `cardStore` was live-only state
/// populated by WS events; cards vanished on page refresh
/// because the store started empty even though CardOpened/Closed
/// events were durably persisted.
///
/// Replaces the conversation's entire card map (rather than
/// merging) because the projection IS the canonical state — any
/// in-memory drift is by definition stale and should be
/// overwritten. Live WS events that arrive AFTER hydration land
/// via `applyCardEvent` and merge per-card on top.
export function setCardsForConversation(
    conversationId: string,
    cards: ReadonlyArray<Card>,
): void {
    const next: Record<string, Card> = {};
    for (const c of cards) {
        next[c.card_id] = c;
    }
    setState({
        byConversation: {
            ...state.byConversation,
            [conversationId]: next,
        },
    });
}

/// Test seam: drop everything. Production callers don't need this.
export function __resetCardStore(): void {
    state = { byConversation: {} };
    emit();
}
