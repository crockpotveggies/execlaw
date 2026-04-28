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
import { Navigate, useNavigate, useParams } from "react-router-dom";
import { useScreenTransition } from "../anim/useScreenTransition";
import { ApiError } from "../api/client";
import {
    getAlertCount,
    listMessages,
    listPendingApprovals,
    listThreads,
    listUiPanels,
    patchThread,
    postGenerateTitle,
    postIncognitoTurn,
    postMessage,
    postStopTurn,
    respondApproval,
    type UiPanelSummary,
} from "../api/endpoints";
import { WsClient, type WsEvent } from "../api/ws";
import { useAuth } from "../auth/AuthContext";
import { ApprovalCard } from "../chat/ApprovalCard";
import { Composer } from "../chat/Composer";
import { MessageStream } from "../chat/MessageStream";
import { Sidebar } from "../chat/Sidebar";
import { useVoiceReadiness } from "../chat/useVoiceReadiness";
import { VoicePlayback, type VoiceAudioOutbound } from "../chat/VoicePlayback";
import { VoiceStatusBar } from "../chat/VoiceStatusBar";
import { WelcomeView } from "../chat/WelcomeView";
import {
    appendMessage,
    appendStreamingToken,
    clearIncognitoMessages,
    clearPendingApproval,
    clearSendingThread,
    clearStreamingBuffer,
    getChatState,
    markSendingThread,
    markUnread,
    setActiveThread,
    setAlertFiringCount,
    setMessages,
    setPendingApprovals,
    setThreadProcessing,
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
    const navigate = useNavigate();
    const { conversationId: routeConversationId } = useParams();
    const activeId = useChatState((s) => s.activeId);
    const [topError, setTopError] = useState<string | null>(null);

    // 2026-04-28 — incognito mode. When true, the next send mints a
    // client-only conversation (id prefix `incognito:`) that lives
    // entirely in the browser; no `state_events` / `state_conversations`
    // rows, no sidebar entry, no URL routing. Navigating away
    // (route change) wipes the session and returns the toggle to
    // off, matching the standard "incognito means it's gone when
    // you close the tab" mental model.
    const [incognito, setIncognito] = useState(false);

    // 2026-04-28 — URL → store sync. The `/chat/:conversationId`
    // route should drive the active thread, so a deep-link or
    // browser-back lands on the right conversation. Mirrors any
    // route change into the chat store; the inverse direction
    // (store → URL) lives at the call sites that activate threads
    // (sidebar click, onNewThread, onSend mint). Incognito
    // sessions are EXEMPT — their id never lands in the URL, and
    // a route change (back to /chat or to a real thread) tears the
    // incognito session down rather than letting it linger as
    // ghost state.
    useEffect(() => {
        const next = routeConversationId ?? null;
        const isIncognitoActive =
            typeof activeId === "string" && activeId.startsWith("incognito:");
        if (isIncognitoActive && next !== activeId) {
            // Operator navigated away from an in-flight incognito
            // chat — drop the messages and reset the toggle.
            clearIncognitoMessages(activeId);
            setIncognito(false);
        }
        if (next !== activeId) {
            setActiveThread(next);
        }
    }, [routeConversationId, activeId]);
    const [uiPanels, setUiPanels] = useState<UiPanelSummary[] | null>(null);
    const wsRef = useRef<WsClient | null>(null);
    /// Phase 13.C — VoicePlayback singleton for the chat shell.
    /// Lazily constructed on the first VoiceAudioOutbound event so
    /// browsers that gate AudioContext on a user gesture don't error
    /// before the operator's first mic press.
    const playbackRef = useRef<VoicePlayback | null>(null);
    /// Live voice transcript per session — keeps the SPA's "still
    /// listening…" indicator + commits the final text once the
    /// server flushes Whisper.
    const [voiceTranscript, setVoiceTranscript] = useState<{
        session: string;
        text: string;
        is_final: boolean;
    } | null>(null);

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
            // Release the AudioContext when the chat shell unmounts.
            if (playbackRef.current) {
                playbackRef.current.close();
                playbackRef.current = null;
            }
        };
    }, [auth.status, getToken]);

    const onNewThread = useCallback(() => {
        // Resetting the active thread to null routes the main pane
        // back to the welcome view. The actual mint happens lazily on
        // first send so abandoned "new chat" clicks don't litter the
        // server with empty threads.
        if (
            typeof activeId === "string" &&
            activeId.startsWith("incognito:")
        ) {
            clearIncognitoMessages(activeId);
        }
        setIncognito(false);
        setActiveThread(null);
        navigate("/chat");
    }, [navigate, activeId]);

    const onSend = useCallback(
        async (text: string) => {
            // 2026-04-28 — incognito branch. Skip the URL push, the
            // event log, and the thread list — the entire
            // conversation lives in the in-memory `messages` map
            // until the operator navigates away.
            if (incognito) {
                const targetId =
                    typeof activeId === "string" &&
                    activeId.startsWith("incognito:")
                        ? activeId
                        : `incognito:${mintConversationId()}`;
                if (targetId !== activeId) {
                    setActiveThread(targetId);
                }
                // Optimistic user message. Use a monotonic seq so the
                // message order matches insertion even though there's
                // no server-assigned canonical seq.
                const userSeq = Date.now();
                appendMessage(targetId, {
                    seq: userSeq,
                    kind: "user_msg",
                    text,
                    actor: auth.user?.user_id ?? null,
                    committed_at: Math.floor(Date.now() / 1000),
                });
                markSendingThread(targetId);
                // Build the request history from the existing
                // in-memory transcript (excluding the message we
                // JUST appended — we send that separately as `text`).
                // Read straight off the store snapshot rather than
                // via the hook (this is a callback, not a render).
                const prior = (
                    getChatState().messages[targetId] ?? []
                )
                    .filter((m) => m.seq !== userSeq)
                    .filter(
                        (m) => m.kind === "user_msg" || m.kind === "model_turn",
                    )
                    .map((m) => ({
                        role: (m.kind === "user_msg" ? "user" : "assistant") as
                            | "user"
                            | "assistant",
                        content: m.text ?? "",
                    }));
                try {
                    const r = await postIncognitoTurn(
                        { messages: prior, text },
                        getToken,
                        {
                            onDelta: (delta) => {
                                // Live-stream into the per-thread
                                // streaming buffer; the existing
                                // MessageStream component already
                                // renders that buffer below the
                                // committed messages, so incognito
                                // chats animate in just like
                                // regular ones.
                                appendStreamingToken(targetId, delta);
                            },
                        },
                    );
                    appendMessage(targetId, {
                        seq: userSeq + 1,
                        kind: "model_turn",
                        text: r.text,
                        actor: "agent",
                        committed_at: Math.floor(Date.now() / 1000),
                    });
                    clearStreamingBuffer(targetId);
                } catch (e) {
                    // If the stream errored mid-way, drop any
                    // partial buffer so we don't leave half a
                    // reply hanging under the user's message.
                    clearStreamingBuffer(targetId);
                    setTopError(
                        e instanceof Error ? e.message : "incognito send failed",
                    );
                } finally {
                    clearSendingThread(targetId);
                }
                return;
            }

            // Lazy-mint a fresh ConversationId on first send when no
            // thread is active. Welcome → first message lands on a
            // brand-new thread, server-side ensure_conversation creates
            // the row, listThreads picks it up.
            const targetId = activeId ?? mintConversationId();
            if (!activeId) {
                setActiveThread(targetId);
                // Push the new id into the URL so deep-link / browser
                // back works once the thread exists. `replace: true`
                // avoids cluttering history with the welcome view we
                // just left.
                navigate(`/chat/${encodeURIComponent(targetId)}`, {
                    replace: true,
                });
            }

            // 2026-04-28 — flag the thread as "sending" in the store
            // BEFORE the optimistic appendMessage runs. The
            // appendMessage triggers a parent remount (WelcomeView →
            // ActiveThreadPane swap on the first message); that new
            // Composer reads `sendingThreads` from the store on
            // mount, so the stop button is up immediately rather
            // than flickering through one frame of "send" before the
            // store flips. Cleared in the `finally` below.
            markSendingThread(targetId);

            // Optimistic user_msg — the server will return the canonical
            // seq, which appendMessage de-duplicates on.
            appendMessage(targetId, {
                seq: Date.now(),
                kind: "user_msg",
                text,
                actor: auth.user?.user_id ?? null,
                committed_at: Math.floor(Date.now() / 1000),
            });
            // Capture whether THIS turn is the conversation's first
            // — used to fire title generation after the round-trip
            // lands. Server is idempotent so a double-fire is fine,
            // but firing only on first-turn keeps the sidebar from
            // momentarily flickering as the model re-titles a
            // long-running chat.
            const isFirstTurn = !activeId;
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
                // 2026-04-28 — async title generation. Fire-and-
                // forget so the operator doesn't wait on a second
                // model round-trip; refresh the thread list once
                // it lands so the new label shows up in the
                // sidebar without a manual reload.
                if (isFirstTurn) {
                    void postGenerateTitle(targetId, getToken)
                        .then((r) => {
                            if (!r.skipped) {
                                return listThreads(getToken).then((tr) => {
                                    setThreads(tr.threads);
                                });
                            }
                        })
                        .catch((e) => {
                            console.warn("title generation failed", e);
                        });
                }
                void resp;
            } catch (e) {
                setTopError(
                    e instanceof Error ? e.message : "send failed",
                );
            } finally {
                clearSendingThread(targetId);
            }
        },
        [activeId, auth.user, getToken, navigate, incognito],
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
            case "conversation_phase_changed":
                // Phase 10.1: server's authoritative typing /
                // processing indicator. The processing set is
                // {Thinking, AwaitingTool} — those are the only two
                // serialized phase strings we treat as "agent busy."
                // Anything else (idle / awaiting_*, trust_revoked)
                // clears the indicator. Mirrors `Phase::is_processing`
                // on the Rust side so the SPA flag is cross-tab and
                // covers inbound transport messages this tab never
                // originated.
                if (cid && typeof ev.phase === "string") {
                    const processing =
                        ev.phase === "thinking" ||
                        ev.phase === "awaiting_tool";
                    setThreadProcessing(cid, processing);
                    // 2026-04-28 — when the server transitions to a
                    // non-processing phase (Idle, AwaitingTrustDecision,
                    // etc.), clear our local "sending" flag too. The
                    // `finally` in onSend already does this when the
                    // POST resolves, but in races where the WS event
                    // beats the HTTP response (or the request was
                    // cancelled out-of-band) this catches the
                    // remainder so the stop button doesn't get stuck.
                    if (!processing) {
                        clearSendingThread(cid);
                    }
                }
                break;
            // ---- Phase 13.C — voice events --------------------------
            case "voice_transcript": {
                // The server's final transcript for this utterance.
                // Pre-final partials are emitted by future streaming
                // adapters; v1 ships only `is_final: true`.
                const session = typeof ev.session === "string" ? ev.session : "";
                const text = typeof ev.text === "string" ? ev.text : "";
                const isFinal = ev.is_final === true;
                if (session) {
                    setVoiceTranscript({ session, text, is_final: isFinal });
                }
                break;
            }
            case "voice_audio_outbound": {
                // Server is streaming TTS audio. Lazily instantiate
                // the VoicePlayback queue and enqueue the chunk.
                if (!playbackRef.current) {
                    playbackRef.current = new VoicePlayback(24_000);
                }
                playbackRef.current.enqueue(ev as unknown as VoiceAudioOutbound);
                break;
            }
            case "voice_interrupted": {
                // Operator (or server-side VAD) interrupted the
                // agent's reply. Flush playback + clear the
                // transcript banner so the operator sees the
                // pipeline reset.
                if (playbackRef.current) {
                    playbackRef.current.flush();
                }
                setVoiceTranscript(null);
                break;
            }
            case "voice_session_ended": {
                // Drop the playback queue's pending chunks — the
                // session is over.
                if (playbackRef.current) {
                    playbackRef.current.flush();
                }
                break;
            }
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
                <ChatPane
                    activeId={activeId}
                    onSend={onSend}
                    incognito={incognito}
                    onToggleIncognito={() => setIncognito((v) => !v)}
                    onStop={() => {
                        // 2026-04-28 — fire-and-forget stop. The
                        // server is idempotent. WelcomeView's
                        // composer reaches this with whatever
                        // active thread is current at click time —
                        // typically the one onSend just minted.
                        const id = activeId;
                        if (!id) return;
                        void postStopTurn(id, getToken).catch((e) => {
                            console.warn("stop turn failed", e);
                        });
                    }}
                    sendVoiceFrame={(bytes) =>
                        wsRef.current?.sendBinary(bytes) ?? false
                    }
                    sendVoiceControl={(payload) =>
                        wsRef.current?.sendText(payload) ?? false
                    }
                    voiceTranscript={voiceTranscript}
                />
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
    onStop,
    incognito,
    onToggleIncognito,
    sendVoiceFrame,
    sendVoiceControl,
    voiceTranscript,
}: {
    activeId: string | null;
    onSend: (text: string) => Promise<void> | void;
    onStop: () => void;
    incognito: boolean;
    onToggleIncognito: () => void;
    sendVoiceFrame: (bytes: ArrayBuffer) => boolean;
    sendVoiceControl: (payload: object) => boolean;
    voiceTranscript: {
        session: string;
        text: string;
        is_final: boolean;
    } | null;
}) {
    const messages = useChatState((s) =>
        activeId ? s.messages[activeId] ?? null : null,
    );
    const streaming = useChatState((s) =>
        activeId ? s.streamingBuffer[activeId] ?? null : null,
    );
    // 2026-04-28 — same store-backed sending flag the
    // ActiveThreadPane reads, but evaluated on `activeId` so the
    // welcome view's composer can also surface a stop button while
    // the mint-then-send dance runs.
    const isSendingActive = useChatState(
        (s) => activeId !== null && !!s.sendingThreads[activeId],
    );
    const hasContent =
        activeId !== null &&
        ((messages?.length ?? 0) > 0 || (streaming?.length ?? 0) > 0);

    if (!hasContent) {
        return (
            <>
                <VoiceStatusBar
                    transcript={voiceTranscript}
                    sendVoiceControl={sendVoiceControl}
                />
                <WelcomeView
                    onSend={onSend}
                    sendVoiceFrame={sendVoiceFrame}
                    sendVoiceControl={sendVoiceControl}
                    onStop={onStop}
                    busy={isSendingActive}
                    incognito={incognito}
                    onToggleIncognito={onToggleIncognito}
                />
            </>
        );
    }
    return (
        <>
            <VoiceStatusBar
                transcript={voiceTranscript}
                sendVoiceControl={sendVoiceControl}
            />
            <ActiveThreadPane
                conversationId={activeId!}
                onSend={onSend}
                sendVoiceFrame={sendVoiceFrame}
                sendVoiceControl={sendVoiceControl}
            />
        </>
    );
}

function ActiveThreadPane({
    conversationId,
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
}: {
    conversationId: string;
    onSend: (text: string) => Promise<void> | void;
    sendVoiceFrame: (bytes: ArrayBuffer) => boolean;
    sendVoiceControl: (payload: object) => boolean;
}) {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    // Poll voice backend readiness so the composer's mic icon
    // either un-mutes (Voice STT + TTS Healthy) or shows the
    // muted icon + tooltip explaining what's missing.
    const voiceReadiness = useVoiceReadiness(getToken);

    const thread = useChatState((s) =>
        s.threads.find((t) => t.conversation_id === conversationId),
    );
    // 2026-04-28 — outbound `postMessage` in flight for this thread?
    // Drives the composer's stop-button visibility. Lives in the
    // store rather than the Composer's local state because the
    // first-message path remounts Composer mid-await (see chat/store.ts).
    const isSending = useChatState(
        (s) => !!s.sendingThreads[conversationId],
    );
    const approval = useChatState(
        (s) => s.pendingApprovals[conversationId] ?? null,
    );
    const isControl = conversationId.startsWith("controller-thread:");
    const isIncognito = conversationId.startsWith("incognito:");
    const fallbackLabel = isControl
        ? "Control thread"
        : isIncognito
        ? "Incognito chat"
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

    // 2026-04-28 — per-thread incognito toggle removed. Existing
    // chats can no longer be flipped to incognito after-the-fact.
    // Incognito is now a *creation-time* mode on the welcome screen
    // (see WelcomeView's incognito toggle) — once a chat exists in
    // the event log, it stays in the event log.
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
                        onClick={() =>
                            !isControl && !isIncognito && setEditing(true)
                        }
                        role={!isControl && !isIncognito ? "button" : undefined}
                        tabIndex={isControl || isIncognito ? -1 : 0}
                        title={
                            isControl
                                ? "Control thread title is fixed"
                                : isIncognito
                                ? "Incognito chats can't be renamed (nothing's saved)"
                                : "Click to rename"
                        }
                        data-testid="thread-rename-trigger"
                    >
                        {headerLabel}
                    </h2>
                )}
                {/* 2026-04-28 — incognito badge surfaces for
                    client-only sessions (id prefix `incognito:`)
                    so the operator never loses track of whether
                    the active chat will persist. The legacy
                    per-thread is_ephemeral toggle was removed —
                    incognito is now strictly a creation-time
                    choice from the welcome view. */}
                {(thread?.is_ephemeral ||
                    conversationId.startsWith("incognito:")) && (
                    <span className="execlaw-incognito-banner">
                        <i className="bi bi-incognito" aria-hidden />
                        Incognito
                    </span>
                )}
            </header>

            <MessageStream conversationId={conversationId} />

            <div className="execlaw-composer">
                <ApprovalCard
                    approval={approval}
                    busy={approvalBusy}
                    onRespond={(id, verb) => void onApprovalRespond(id, verb)}
                />
                <Composer
                    onSend={onSend}
                    sendVoiceFrame={sendVoiceFrame}
                    sendVoiceControl={sendVoiceControl}
                    voiceReadiness={voiceReadiness}
                    busy={isSending || (thread?.is_processing ?? false)}
                    onStop={() => {
                        // 2026-04-28 — POST /api/chats/:id/stop. Fire-and-
                        // forget: server is idempotent. We DON'T clear
                        // is_processing locally — wait for the server's
                        // ConversationPhaseChanged{phase=idle} event to
                        // do that, otherwise a stale "stopped" state
                        // could overlap with a turn that finished
                        // naturally on its own.
                        void postStopTurn(conversationId, getToken).catch((e) => {
                            // Swallow errors — the operator already
                            // sees the typing indicator; if the stop
                            // fails the turn will eventually finish
                            // (or hit max_tool_rounds). Logging
                            // without a banner.
                            console.warn("stop turn failed", e);
                        });
                    }}
                />
            </div>
        </>
    );
}

