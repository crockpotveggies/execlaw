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

import { useCallback, useEffect, useRef, useState } from "react";
import { Navigate } from "react-router-dom";
import { useScreenTransition } from "../anim/useScreenTransition";
import { ApiError } from "../api/client";
import {
    getAlertCount,
    listMessages,
    listPendingApprovals,
    listThreads,
    listUiPanels,
    patchThread,
    postMessage,
    respondApproval,
    type UiPanelSummary,
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
    clearPendingApproval,
    clearStreamingBuffer,
    markUnread,
    setActiveThread,
    setAlertFiringCount,
    setMessages,
    setPendingApprovals,
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
    const [uiPanels, setUiPanels] = useState<UiPanelSummary[] | null>(null);
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

    // Initial + live thread list + pending approvals + plugin UI panels
    // + firing-alert count for the sidebar badge.
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        let cancelled = false;
        (async () => {
            try {
                const [threadsResp, approvalsResp, panelsResp, alertCount] =
                    await Promise.all([
                        listThreads(getToken),
                        listPendingApprovals(getToken),
                        listUiPanels(getToken),
                        getAlertCount(getToken).catch(() => ({
                            firing_count: 0,
                        })),
                    ]);
                if (!cancelled) {
                    setThreads(threadsResp.threads);
                    setPendingApprovals(approvalsResp.approvals);
                    setUiPanels(panelsResp.panels);
                    setAlertFiringCount(alertCount.firing_count);
                }
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

    // Cheap firing-count poll every 60s so the badge tracks alerts
    // that arrive while the user is sitting on the chat shell. The
    // dedicated AlertsPage refreshes its own list when opened; this
    // poll is just for the sidebar indicator. Switch to WS-pushed
    // alert events once the alert bus lands (§10.8).
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        const id = window.setInterval(async () => {
            try {
                const r = await getAlertCount(getToken);
                setAlertFiringCount(r.firing_count);
            } catch {
                // Silent — a transient failure shouldn't pollute the UI.
            }
        }, 60_000);
        return () => window.clearInterval(id);
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

    // Live event stream. The accessor is read on every reconnect
    // so a token rotation (silent retry / background refresh)
    // propagates to the next WS handshake — without that, a
    // backend restart leaves the WS stuck retrying with the stale
    // pre-restart token forever.
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        const client = new WsClient({
            accessToken: getToken,
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
            case "alert_fired":
            case "alert_resolved":
                // §10.8 live alert pipeline — a freshly fired or
                // ack/resolved alert should bump the badge without
                // waiting for the 60s poll. We don't trust the local
                // count math (deduplication on the server side could
                // make a "fired" event a no-op for an existing
                // fingerprint), so re-query the canonical count.
                getAlertCount(getToken)
                    .then((r) => setAlertFiringCount(r.firing_count))
                    .catch(() => {});
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
            <Sidebar
                    onNewThread={onNewThread}
                    onSignOut={handleSignOut}
                    uiPanels={uiPanels}
                />
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
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const thread = useChatState((s) =>
        s.threads.find((t) => t.conversation_id === conversationId),
    );
    const approval = useChatState(
        (s) => s.pendingApprovals[conversationId] ?? null,
    );
    const isControl = conversationId.startsWith("controller-thread:");
    const fallbackLabel = isControl
        ? "Control thread"
        : `New chat · ${conversationId.slice(0, 6)}`;
    const headerLabel = thread?.display_name ?? fallbackLabel;

    const [editing, setEditing] = useState(false);
    const [editValue, setEditValue] = useState(headerLabel);
    const [approvalBusy, setApprovalBusy] = useState(false);

    // Reset edit buffer whenever the active thread / its label changes.
    useEffect(() => {
        setEditValue(headerLabel);
        setEditing(false);
    }, [conversationId, headerLabel]);

    const commitRename = useCallback(async () => {
        const trimmed = editValue.trim();
        setEditing(false);
        if (!trimmed || trimmed === headerLabel) return;
        try {
            await patchThread(
                conversationId,
                { display_name: trimmed },
                getToken,
            );
            // Refresh the thread list so the sidebar picks up the new name.
            const r = await listThreads(getToken);
            setThreads(r.threads);
        } catch {
            /* swallow — surfaced via the chat error banner if it's persistent */
        }
    }, [conversationId, editValue, getToken, headerLabel]);

    const toggleIncognito = useCallback(async () => {
        if (!thread) return;
        try {
            if (thread.is_ephemeral) {
                await patchThread(
                    conversationId,
                    { is_ephemeral: false },
                    getToken,
                );
            } else {
                // Default 1-hour expiry — matches MIGRATION_PLAN §2.6.
                const expiresAt =
                    Math.floor(Date.now() / 1000) + 60 * 60;
                await patchThread(
                    conversationId,
                    {
                        is_ephemeral: true,
                        ephemeral_expires_at: expiresAt,
                    },
                    getToken,
                );
            }
            const r = await listThreads(getToken);
            setThreads(r.threads);
        } catch {
            /* swallow */
        }
    }, [conversationId, getToken, thread]);

    const onApprovalRespond = useCallback(
        async (
            approvalId: string,
            verb: "Trust" | "TrustLimited" | "Block" | "TrustOnce",
        ) => {
            setApprovalBusy(true);
            try {
                await respondApproval(approvalId, { verb }, getToken);
                clearPendingApproval(conversationId);
                // Refresh threads so kind/trust_class on the sidebar
                // updates immediately.
                const r = await listThreads(getToken);
                setThreads(r.threads);
            } catch {
                /* swallow — operator can retry */
            } finally {
                setApprovalBusy(false);
            }
        },
        [conversationId, getToken],
    );

    return (
        <>
            <header className="execlaw-main__head">
                {editing ? (
                    <input
                        autoFocus
                        className="execlaw-thread-rename"
                        value={editValue}
                        onChange={(e) => setEditValue(e.target.value)}
                        onBlur={() => void commitRename()}
                        onKeyDown={(e) => {
                            if (e.key === "Enter") {
                                e.preventDefault();
                                void commitRename();
                            }
                            if (e.key === "Escape") {
                                setEditing(false);
                                setEditValue(headerLabel);
                            }
                        }}
                        data-testid="thread-rename-input"
                    />
                ) : (
                    <h2
                        className="h6 mb-0 execlaw-thread-rename"
                        onClick={() => !isControl && setEditing(true)}
                        role={!isControl ? "button" : undefined}
                        tabIndex={isControl ? -1 : 0}
                        title={
                            isControl
                                ? "Control thread title is fixed"
                                : "Click to rename"
                        }
                        data-testid="thread-rename-trigger"
                    >
                        {headerLabel}
                    </h2>
                )}
                {thread?.is_ephemeral && (
                    <span className="badge bg-secondary ms-2">incognito</span>
                )}
                {!isControl && (
                    <button
                        type="button"
                        className="btn btn-link btn-sm p-1 ms-auto execlaw-muted"
                        onClick={() => void toggleIncognito()}
                        aria-label={
                            thread?.is_ephemeral
                                ? "Disable incognito"
                                : "Make incognito"
                        }
                        title={
                            thread?.is_ephemeral
                                ? "Disable incognito"
                                : "Make incognito (purges in ~1h)"
                        }
                        data-testid="thread-incognito-toggle"
                    >
                        <i
                            className={
                                "bi " +
                                (thread?.is_ephemeral
                                    ? "bi-incognito"
                                    : "bi-eye-slash")
                            }
                            aria-hidden
                        />
                    </button>
                )}
            </header>

            <MessageStream conversationId={conversationId} />

            <div className="execlaw-composer">
                <ApprovalCard
                    approval={approval}
                    busy={approvalBusy}
                    onRespond={(id, verb) => void onApprovalRespond(id, verb)}
                />
                <Composer onSend={onSend} />
            </div>
        </>
    );
}

