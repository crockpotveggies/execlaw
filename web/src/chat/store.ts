// Chat store — minimal in-tree state machine for the SPA's thread
// list + active-thread message stream. No external state library: the
// surface today is small, and React's `useSyncExternalStore` lets us
// expose a clean subscribe/getState API without a Provider tree.
//
// As features accumulate (multiple selectors, persisted UI state) we
// can swap to Zustand without touching components — they consume via
// the typed hooks below.

import { useSyncExternalStore } from "react";
import type {
    MessageView,
    PendingApprovalSummary,
    ThreadSummary,
} from "../api/endpoints";

interface ThreadView extends ThreadSummary {
    /** Local-only flag: agent has replied since the user last opened this thread. */
    has_unread: boolean;
    /** Local-only flag: agent is currently producing a reply on this thread. */
    is_thinking: boolean;
}

export interface ChatState {
    threads: ThreadView[];
    activeId: string | null;
    /** Messages keyed by conversation id. Loaded lazily on activation. */
    messages: Record<string, MessageView[]>;
    /** Token-stream buffer for the active thread (the SSE/WS feed
     *  appends; flushed when the final assistant message lands). */
    streamingBuffer: Record<string, string>;
    /** Pending cold-contact approvals keyed by conversation id. */
    pendingApprovals: Record<string, PendingApprovalSummary>;
}

type Listener = () => void;

const listeners = new Set<Listener>();
let state: ChatState = {
    threads: [],
    activeId: null,
    messages: {},
    streamingBuffer: {},
    pendingApprovals: {},
};

function emit() {
    for (const l of listeners) l();
}

function setState(updater: (prev: ChatState) => ChatState) {
    const next = updater(state);
    if (next !== state) {
        state = next;
        emit();
    }
}

// Public API ---------------------------------------------------------

export function getChatState(): ChatState {
    return state;
}

export function subscribe(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
}

export function useChatState<T>(selector: (s: ChatState) => T): T {
    return useSyncExternalStore(
        subscribe,
        () => selector(state),
        () => selector(state),
    );
}

/**
 * Replace the thread list (server is the source of truth on each
 * fetch). Local UI flags are merged from the previous list.
 */
export function setThreads(next: ThreadSummary[]) {
    setState((prev) => {
        const localFlags = new Map<string, { has_unread: boolean; is_thinking: boolean }>();
        for (const t of prev.threads) {
            localFlags.set(t.conversation_id, {
                has_unread: t.has_unread,
                is_thinking: t.is_thinking,
            });
        }
        return {
            ...prev,
            threads: next.map((t) => {
                const flags = localFlags.get(t.conversation_id);
                return {
                    ...t,
                    has_unread: flags?.has_unread ?? false,
                    is_thinking: flags?.is_thinking ?? false,
                };
            }),
        };
    });
}

export function setActiveThread(conversationId: string | null) {
    setState((prev) => {
        if (prev.activeId === conversationId) return prev;
        // Opening a thread clears its unread + thinking flags.
        const threads = prev.threads.map((t) =>
            t.conversation_id === conversationId
                ? { ...t, has_unread: false }
                : t,
        );
        return { ...prev, activeId: conversationId, threads };
    });
}

export function setMessages(conversationId: string, messages: MessageView[]) {
    setState((prev) => ({
        ...prev,
        messages: { ...prev.messages, [conversationId]: messages },
    }));
}

export function appendMessage(conversationId: string, message: MessageView) {
    setState((prev) => {
        const existing = prev.messages[conversationId] ?? [];
        // Idempotent on duplicate seq: replace, don't double-append.
        const without = existing.filter((m) => m.seq !== message.seq);
        const next = [...without, message].sort((a, b) => a.seq - b.seq);
        return {
            ...prev,
            messages: { ...prev.messages, [conversationId]: next },
        };
    });
}

export function appendStreamingToken(conversationId: string, token: string) {
    setState((prev) => {
        const buf = prev.streamingBuffer[conversationId] ?? "";
        return {
            ...prev,
            streamingBuffer: {
                ...prev.streamingBuffer,
                [conversationId]: buf + token,
            },
            threads: prev.threads.map((t) =>
                t.conversation_id === conversationId
                    ? { ...t, is_thinking: true }
                    : t,
            ),
        };
    });
}

export function clearStreamingBuffer(conversationId: string) {
    setState((prev) => {
        if (!(conversationId in prev.streamingBuffer)) return prev;
        const { [conversationId]: _drop, ...rest } = prev.streamingBuffer;
        void _drop;
        return {
            ...prev,
            streamingBuffer: rest,
            threads: prev.threads.map((t) =>
                t.conversation_id === conversationId
                    ? { ...t, is_thinking: false }
                    : t,
            ),
        };
    });
}

export function markUnread(conversationId: string) {
    setState((prev) => ({
        ...prev,
        threads: prev.threads.map((t) =>
            t.conversation_id === conversationId
                ? { ...t, has_unread: true }
                : t,
        ),
    }));
}

/**
 * Replace the pending-approvals map. Server is the source of truth on
 * each fetch; we key by conversation_id so the chat shell can show
 * the inline approval card without a per-thread query.
 */
export function setPendingApprovals(items: PendingApprovalSummary[]) {
    setState((prev) => {
        const next: Record<string, PendingApprovalSummary> = {};
        for (const a of items) {
            next[a.conversation_id] = a;
        }
        return { ...prev, pendingApprovals: next };
    });
}

export function clearPendingApproval(conversationId: string) {
    setState((prev) => {
        if (!(conversationId in prev.pendingApprovals)) return prev;
        const { [conversationId]: _drop, ...rest } = prev.pendingApprovals;
        void _drop;
        return { ...prev, pendingApprovals: rest };
    });
}

/** Test seam: reset the entire store. Production code never calls this. */
export function __resetChatStore() {
    state = {
        threads: [],
        activeId: null,
        messages: {},
        streamingBuffer: {},
        pendingApprovals: {},
    };
    emit();
}
