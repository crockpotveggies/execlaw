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
    /**
     * Local-only flag: agent is hot-path-busy on this thread —
     * driven by the server's `ConversationPhaseChanged` event when
     * the phase enters `Thinking` / `AwaitingTool` (i.e. the
     * `is_processing()` set on the Rust side). Surfaced as the
     * sidebar's typing-spinner and as the typing-indicator hook
     * that transports use to drive Signal/email "is typing" UX.
     */
    is_processing: boolean;
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
    /** Count of alerts in `Firing` status. Surfaced in the sidebar
     *  badge so the operator notices an active alert without having
     *  to open the page. */
    alertFiringCount: number;
    /**
     * 2026-04-28 — set of conversation ids whose `postMessage` is
     * still in flight from THIS tab. Survives component remount
     * (WelcomeView → ActiveThreadPane swap on first message), unlike
     * the Composer's local `submitting` state. The Composer's stop
     * button reads this so the affordance is available the whole
     * time the operator is waiting on a reply, even before the
     * server's first WS phase event lands.
     */
    sendingThreads: Record<string, true>;
}

type Listener = () => void;

const listeners = new Set<Listener>();
let state: ChatState = {
    threads: [],
    activeId: null,
    messages: {},
    streamingBuffer: {},
    pendingApprovals: {},
    alertFiringCount: 0,
    sendingThreads: {},
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
        const localFlags = new Map<string, { has_unread: boolean; is_processing: boolean }>();
        for (const t of prev.threads) {
            localFlags.set(t.conversation_id, {
                has_unread: t.has_unread,
                is_processing: t.is_processing,
            });
        }
        return {
            ...prev,
            threads: next.map((t) => {
                const flags = localFlags.get(t.conversation_id);
                return {
                    ...t,
                    has_unread: flags?.has_unread ?? false,
                    is_processing: flags?.is_processing ?? false,
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

/// 2026-04-28 — drop ALL local state for a conversation id. Used
/// when an incognito session is being torn down so its transcript
/// doesn't linger in the store after the operator navigates away.
/// Also clears the streaming buffer + sending flag since none of
/// those should outlive the conversation either.
export function clearIncognitoMessages(conversationId: string) {
    setState((prev) => {
        const {
            [conversationId]: _msgs,
            ...remainingMessages
        } = prev.messages;
        const {
            [conversationId]: _buf,
            ...remainingBuffer
        } = prev.streamingBuffer;
        const {
            [conversationId]: _sending,
            ...remainingSending
        } = prev.sendingThreads;
        void _msgs;
        void _buf;
        void _sending;
        return {
            ...prev,
            messages: remainingMessages,
            streamingBuffer: remainingBuffer,
            sendingThreads: remainingSending,
        };
    });
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
                    ? { ...t, is_processing: true }
                    : t,
            ),
        };
    });
}

/**
 * Set the `is_processing` flag on a thread directly. Driven by the
 * server's `ConversationPhaseChanged` WS event so the indicator
 * works cross-tab and for inbound transport messages that this tab
 * never originated. Idempotent.
 */
/**
 * 2026-04-28 — mark a thread as having an outbound `postMessage` in
 * flight. Survives a parent remount (WelcomeView → ActiveThreadPane
 * swap when the first user message lands). Clear with
 * `clearSendingThread` when the await resolves OR when the server's
 * `phase=idle` event arrives, whichever lands first.
 */
export function markSendingThread(conversationId: string) {
    setState((prev) => {
        if (prev.sendingThreads[conversationId]) return prev;
        return {
            ...prev,
            sendingThreads: { ...prev.sendingThreads, [conversationId]: true },
        };
    });
}

export function clearSendingThread(conversationId: string) {
    setState((prev) => {
        if (!(conversationId in prev.sendingThreads)) return prev;
        const { [conversationId]: _drop, ...rest } = prev.sendingThreads;
        void _drop;
        return { ...prev, sendingThreads: rest };
    });
}

export function setThreadProcessing(
    conversationId: string,
    processing: boolean,
) {
    setState((prev) => {
        let changed = false;
        const threads = prev.threads.map((t) => {
            if (t.conversation_id !== conversationId) return t;
            if (t.is_processing === processing) return t;
            changed = true;
            return { ...t, is_processing: processing };
        });
        return changed ? { ...prev, threads } : prev;
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
                    ? { ...t, is_processing: false }
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

/** Set the firing-alerts count surfaced by the sidebar badge. */
export function setAlertFiringCount(count: number) {
    setState((prev) =>
        prev.alertFiringCount === count
            ? prev
            : { ...prev, alertFiringCount: count },
    );
}

/** Test seam: reset the entire store. Production code never calls this. */
export function __resetChatStore() {
    state = {
        threads: [],
        activeId: null,
        messages: {},
        streamingBuffer: {},
        pendingApprovals: {},
        alertFiringCount: 0,
        sendingThreads: {},
    };
    emit();
}
