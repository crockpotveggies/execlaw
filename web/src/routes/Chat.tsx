// Chat shell — sidebar + main content area. This is the load-bearing
// route post-login. Wires:
//
//   - GET /api/chats on mount → seeds the thread list,
//   - new-chat button → mints a UUID-based ConversationId locally,
//     activates it, and the next sent message lands on it,
//   - active thread changes → fetches its message history,
//   - WS /api/stream → tokens append to streaming buffer; thread-state
//     events (created/replied) refresh the list,
//   - Composer → POST /api/chats/:id/messages → optimistic local push.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Navigate } from "react-router-dom";
import { useScreenTransition } from "../anim/useScreenTransition";
import { ApiError } from "../api/client";
import {
    listMessages,
    listThreads,
    postMessage,
} from "../api/endpoints";
import { WsClient, type WsEvent } from "../api/ws";
import { useAuth } from "../auth/AuthContext";
import { ApprovalCard } from "../chat/ApprovalCard";
import { Composer } from "../chat/Composer";
import { MessageStream } from "../chat/MessageStream";
import { Sidebar } from "../chat/Sidebar";
import { WelcomeView } from "../chat/WelcomeView";
import {
    appendMessage,
    appendStreamingToken,
    clearStreamingBuffer,
    markUnread,
    setActiveThread,
    setMessages,
    setThreads,
    useChatState,
} from "../chat/store";

function mintConversationId(): string {
    // Browser crypto is fine for a client-minted thread id; the
    // server treats whatever the client posts as the conversation id
    // for the lifetime of that thread.
    if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
        return crypto.randomUUID();
    }
    return `conv-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

export function Chat() {
    const auth = useAuth();
    const activeId = useChatState((s) => s.activeId);
    const [topError, setTopError] = useState<string | null>(null);
    const wsRef = useRef<WsClient | null>(null);

    // Chat shell fade: opacity-only on both ends. Login screen's
    // scale-up + fade picks up after the shell has fully faded out.
    const { ref: shellRef, dismiss } = useScreenTransition<HTMLDivElement>({
        initialScale: 1,
        exitScale: 1,
        durationMs: 240,
    });

    const handleSignOut = useCallback(() => {
        dismiss(() => {
            void auth.signOut();
        });
    }, [auth, dismiss]);

    // Stable accessor used by everything that needs the live access token.
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    // Initial + live thread list.
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        let cancelled = false;
        (async () => {
            try {
                const resp = await listThreads(getToken);
                if (!cancelled) setThreads(resp.threads);
            } catch (e) {
                if (!cancelled)
                    setTopError(
                        e instanceof Error
                            ? e.message
                            : "couldn't load threads",
                    );
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [auth.status, getToken]);

    // Lazy load messages on active-thread change.
    useEffect(() => {
        if (!activeId) return;
        let cancelled = false;
        (async () => {
            try {
                const resp = await listMessages(activeId, getToken);
                if (!cancelled) setMessages(activeId, resp.messages);
            } catch (e) {
                if (e instanceof ApiError && e.code === "unauthorized") {
                    await auth.signOut();
                    return;
                }
                if (!cancelled)
                    setTopError(
                        e instanceof Error
                            ? e.message
                            : "couldn't load messages",
                    );
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [activeId, auth, getToken]);

    // Live event stream.
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        const client = new WsClient({
            accessToken: getToken(),
            onEvent: (ev) => handleWsEvent(ev),
        });
        client.open();
        wsRef.current = client;
        return () => {
            client.close();
            wsRef.current = null;
        };
    }, [auth.status, getToken]);

    const onNewThread = useCallback(() => {
        // Resetting the active thread to null routes the main pane
        // back to the welcome view. The actual mint happens lazily on
        // first send so abandoned "new chat" clicks don't litter the
        // server with empty threads.
        setActiveThread(null);
    }, []);

    const onSend = useCallback(
        async (text: string) => {
            // Lazy-mint a fresh ConversationId on first send when no
            // thread is active. Welcome → first message lands on a
            // brand-new thread, server-side ensure_conversation creates
            // the row, listThreads picks it up.
            const targetId = activeId ?? mintConversationId();
            if (!activeId) {
                setActiveThread(targetId);
            }

            // Optimistic user_msg — the server will return the canonical
            // seq, which appendMessage de-duplicates on.
            appendMessage(targetId, {
                seq: Date.now(),
                kind: "user_msg",
                text,
                actor: auth.user?.user_id ?? null,
                committed_at: Math.floor(Date.now() / 1000),
            });
            try {
                const resp = await postMessage(targetId, { text }, getToken);
                // Reload the canonical history so seqs are correct.
                const fresh = await listMessages(targetId, getToken);
                setMessages(targetId, fresh.messages);
                clearStreamingBuffer(targetId);
                // Refresh the thread list (last_seq + new threads land here).
                listThreads(getToken)
                    .then((r) => setThreads(r.threads))
                    .catch(() => {});
                void resp;
            } catch (e) {
                setTopError(
                    e instanceof Error ? e.message : "send failed",
                );
            }
        },
        [activeId, auth.user, getToken],
    );

    const handleWsEvent = useCallback((ev: WsEvent) => {
        // Server emits snake_case variants via `#[serde(tag = "kind",
        // rename_all = "snake_case")]` — see crates/server/src/events.rs.
        const cid = typeof ev.conversation_id === "string" ? ev.conversation_id : null;
        switch (ev.kind) {
            case "chat_token_delta":
                if (cid && typeof ev.text === "string") {
                    appendStreamingToken(cid, ev.text);
                }
                break;
            case "chat_message_outbound":
            case "chat_message_inbound":
                if (cid) {
                    clearStreamingBuffer(cid);
                    listThreads(getToken)
                        .then((r) => setThreads(r.threads))
                        .catch(() => {});
                    listMessages(cid, getToken)
                        .then((r) => setMessages(cid, r.messages))
                        .catch(() => {});
                    if (
                        ev.kind === "chat_message_outbound" &&
                        cid !== activeId
                    ) {
                        markUnread(cid);
                    }
                }
                break;
            default:
                // Ignore unknown event kinds — additive event vocabulary.
                break;
        }
    }, [activeId, getToken]);

    if (auth.status === "loading") {
        return (
            <div className="execlaw-auth-shell">
                <div className="execlaw-muted small">Loading session…</div>
            </div>
        );
    }
    if (auth.status === "unauthenticated") {
        return <Navigate to="/login" replace />;
    }

    return (
        <div ref={shellRef} className="execlaw-shell">
            <Sidebar onNewThread={onNewThread} onSignOut={handleSignOut} />
            <main className="execlaw-main">
                {topError && (
                    <div
                        className="execlaw-error-banner mx-3 mt-3"
                        role="alert"
                        data-testid="chat-error-banner"
                    >
                        {topError}
                    </div>
                )}
                <ChatPane activeId={activeId} onSend={onSend} />
            </main>
        </div>
    );
}

/**
 * The main right-hand pane. Shows the welcome view (centered model
 * brand + composer + suggestions) until the active thread has at least
 * one message; flips to the bottom-anchored stream + composer once a
 * conversation is in progress.
 */
function ChatPane({
    activeId,
    onSend,
}: {
    activeId: string | null;
    onSend: (text: string) => Promise<void> | void;
}) {
    const messages = useChatState((s) =>
        activeId ? s.messages[activeId] ?? null : null,
    );
    const streaming = useChatState((s) =>
        activeId ? s.streamingBuffer[activeId] ?? null : null,
    );
    const hasContent =
        activeId !== null &&
        ((messages?.length ?? 0) > 0 || (streaming?.length ?? 0) > 0);

    if (!hasContent) {
        return <WelcomeView onSend={onSend} />;
    }
    return <ActiveThreadPane conversationId={activeId!} onSend={onSend} />;
}

function ActiveThreadPane({
    conversationId,
    onSend,
}: {
    conversationId: string;
    onSend: (text: string) => Promise<void> | void;
}) {
    const thread = useChatState((s) =>
        s.threads.find((t) => t.conversation_id === conversationId),
    );
    const headerLabel = useMemo(() => {
        if (thread?.display_name) return thread.display_name;
        if (conversationId.startsWith("controller-thread:")) return "Control thread";
        return `New chat · ${conversationId.slice(0, 6)}`;
    }, [thread, conversationId]);

    return (
        <>
            <header className="execlaw-main__head">
                <h2 className="h6 mb-0">{headerLabel}</h2>
                {thread?.is_ephemeral && (
                    <span className="badge bg-secondary ms-2">incognito</span>
                )}
            </header>

            <MessageStream conversationId={conversationId} />

            <div className="execlaw-composer">
                <ApprovalCard approval={null} />
                <Composer onSend={onSend} />
            </div>
        </>
    );
}

