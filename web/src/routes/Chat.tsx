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
import { useChatTransition } from "../anim/useChatTransition";
import { useScreenTransition } from "../anim/useScreenTransition";
import { ApiError } from "../api/client";
import {
    getAlertCount,
    listCards,
    listMessages,
    listSkills,
    listThreads,
    listUiPanels,
    patchThread,
    postGenerateTitle,
    postMessage,
    postStopTurn,
    respondApproval,
    type ApprovalVerb,
    type InlineAttachment,
    type SkillListEntry,
    type UiPanelSummary,
} from "../api/endpoints";
import { useBackendCapabilities } from "../chat/useBackendCapabilities";
import { WsClient, type WsEvent } from "../api/ws";
import { applyCardEvent, setCardsForConversation } from "../cards/cardStore";
// Side-effect import: registers the ResearchCard renderer for
// `kind: "research"` cards. CardRenderer.tsx already auto-registers
// the generic LongRunningTaskCard fallback at module-load.
import "../cards/ResearchCard";
// Side-effect import: registers the AttachmentCard renderer for
// `kind: "attachment"` cards (the inline file-delivery chip the
// agent emits via `send_attachment`).
import "../cards/AttachmentCard";
// Side-effect import: registers the ChartCard renderer for
// `kind: "chart"` cards. The built-in `chart.render` tool emits
// these alongside its tool_result so the SVG renders inline via
// the same proven cards-projection path as research/attachment.
import "../cards/ChartCard";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
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
    clearToolActivity,
    getChatState,
    markSendingThread,
    markUnread,
    setActiveThread,
    setAlertFiringCount,
    setMessages,
    setThreadProcessing,
    setThreads,
    setToolActivity,
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
    // (sidebar click, onNewThread, onSend mint).
    //
    // Incognito quirk: their id deliberately stays out of the URL
    // (URL stays at `/chat`), so a naive sync would see
    // `routeConversationId=null` vs `activeId="incognito:..."` as
    // a disagreement and immediately clobber the in-flight
    // session. We therefore only wipe an active incognito session
    // when the URL points at a *different real id* (sidebar click)
    // — bare `/chat` is a no-op while the operator is mid-chat in
    // incognito mode.
    useEffect(() => {
        const next = routeConversationId ?? null;
        const isIncognitoActive =
            typeof activeId === "string" && activeId.startsWith("incognito:");
        if (isIncognitoActive) {
            if (next === null || next === activeId) {
                // URL is `/chat` (or already mirrors the incognito
                // id) — keep the session alive, don't touch the
                // store.
                return;
            }
            // URL points at a real thread; operator navigated
            // away. Tear down the incognito session.
            clearIncognitoMessages(activeId);
            setIncognito(false);
            setActiveThread(next);
            return;
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
    const getToken = auth.getAccessToken;

    // Plugin-declared UI panels for the Sidebar's "More" section.
    // Loaded here (rather than inside Sidebar) because the panel set
    // also drives the Composer's slash-command palette below; both
    // surfaces share the prop. Threads / pending approvals / firing-
    // alert count moved into Sidebar itself so a refresh on a non-
    // chat route still populates them (was: only loaded on /chat).
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        let cancelled = false;
        (async () => {
            try {
                const panelsResp = await listUiPanels(getToken);
                if (!cancelled) setUiPanels(panelsResp.panels);
            } catch (e) {
                if (!cancelled)
                    setTopError(
                        e instanceof Error
                            ? e.message
                            : "couldn't load plugin panels",
                    );
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [auth.status, getToken]);

    // Lazy load messages on active-thread change.
    //
    // Skip incognito sessions — there's no event-log row for them
    // server-side, so a GET would just return `[]` and clobber the
    // optimistic user_msg the SPA has in local state. Worse, the
    // GET races the in-flight POST on the same path and on Firefox
    // the concurrent request pair surfaces as a `NetworkError when
    // attempting to fetch resource` on the POST side. Incognito
    // transcripts only exist in the chat store, period.
    useEffect(() => {
        if (!activeId) return;
        if (activeId.startsWith("incognito:")) return;
        let cancelled = false;
        (async () => {
            try {
                // Fire messages + cards in parallel so the chat
                // pane re-hydrates both halves of the timeline at
                // once. Cards used to be live-WS-only state, which
                // meant a page refresh dropped every inline card
                // (research card, attachment chip, etc.) even
                // though the underlying CardOpened/Closed events
                // were durably persisted server-side. The
                // `listCards` projection rebuilds the per-card
                // state from the log; `setCardsForConversation`
                // overwrites the conversation's card map in one
                // shot so the SPA re-renders with the chips back
                // in place.
                const [messagesResp, cardsResp] = await Promise.all([
                    listMessages(activeId, getToken),
                    listCards(activeId, getToken).catch((e) => {
                        // Cards endpoint failure is non-fatal —
                        // messages still render, just without
                        // historical cards. Log so we notice if
                        // it starts happening regularly.
                        // eslint-disable-next-line no-console
                        console.warn("listCards failed:", e);
                        return null;
                    }),
                ]);
                if (cancelled) return;
                setMessages(activeId, messagesResp.messages);
                if (cardsResp) {
                    setCardsForConversation(
                        activeId,
                        cardsResp.cards as unknown as Parameters<
                            typeof setCardsForConversation
                        >[1],
                    );
                }
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

    // 2026-04-28 — pin the live `handleWsEvent` in a ref. The
    // WsClient is constructed ONCE per auth-status flip; if we
    // closed the WS handler over `handleWsEvent` directly, every
    // inbound frame would reach the very first closure (built
    // when `activeId === null` at chat-shell mount). That stale
    // `activeId` was the root cause of the "active chat shows as
    // unread when its reply arrives" bug — `cid !== activeId`
    // was true against the stale null. Reading through this ref
    // on each frame guarantees the handler always sees the latest
    // `activeId` (and any other state the handler closes over).
    // The ref starts null because `handleWsEvent` is declared
    // later in this function (TDZ); we install the live value via
    // useEffect below.
    const handleWsEventRef = useRef<((ev: WsEvent) => void) | null>(null);

    // Live event stream. The accessor is read on every reconnect
    // so a token rotation (silent retry / background refresh)
    // propagates to the next WS handshake — without that, a
    // backend restart leaves the WS stuck retrying with the stale
    // pre-restart token forever.
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        const client = new WsClient({
            accessToken: getToken,
            onEvent: (ev) => {
                handleWsEventRef.current?.(ev);
            },
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
        async (
            text: string,
            attachments: InlineAttachment[] = [],
            skillNames: string[] = [],
        ) => {
            // Lazy-mint a fresh ConversationId on first send when no
            // thread is active. Incognito sends use the same path —
            // they just carry an `incognito: true` flag that tells
            // the server to skip every persistent write (and the SPA
            // marks the id with an `incognito:` prefix so the
            // sidebar / thread list never picks it up + so navigation
            // away can wipe local state).
            const targetId =
                activeId ??
                (incognito
                    ? `incognito:${mintConversationId()}`
                    : mintConversationId());
            if (!activeId) {
                setActiveThread(targetId);
                if (!incognito) {
                    // Push the new id into the URL so deep-link /
                    // browser back works once the thread exists.
                    // Incognito ids deliberately stay out of the
                    // URL (sharable URLs imply persistence, which
                    // is the whole thing incognito refuses).
                    navigate(`/chat/${encodeURIComponent(targetId)}`, {
                        replace: true,
                    });
                }
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
            // seq for regular chats, which appendMessage de-duplicates
            // on. For incognito the seq stays client-only since there's
            // no event-log row to derive a canonical one from.
            //
            // Optimistic attachments: stash the data URL itself in
            // the `id` field. MessageStream renders attachments via
            // `<img src={att.id.startsWith("data:") ? id : `/api/attachments/${id}`}>`,
            // so the operator sees their own image inline immediately
            // — no flicker waiting on the server round-trip. Once
            // `listMessages` lands the optimistic entry is replaced
            // by the canonical row that points at the persisted blob.
            const optimisticSeq = Date.now();
            const optimisticAttachments = attachments.map((a) => ({
                id: a.data_url,
                mime: a.mime,
            }));
            appendMessage(targetId, {
                seq: optimisticSeq,
                kind: "user_msg",
                text,
                actor: auth.user?.user_id ?? null,
                committed_at: Math.floor(Date.now() / 1000),
                attachments: optimisticAttachments,
                // 2026-05-15 — surface the chosen skills as a chip on
                // the user bubble immediately. The canonical row from
                // listMessages will replace this on the seq match.
                applied_skill_names:
                    skillNames.length > 0 ? skillNames : undefined,
            });
            // Capture whether THIS turn is the conversation's first
            // — used to fire title generation after the round-trip
            // lands. Server is idempotent so a double-fire is fine,
            // but firing only on first-turn keeps the sidebar from
            // momentarily flickering as the model re-titles a
            // long-running chat.
            const isFirstTurn = !activeId;
            // For incognito sends the server can't replay the event
            // log — collect the running transcript from the local
            // store and ship it. Read the snapshot synchronously
            // (we're inside a callback, not a render).
            const priorMessages = incognito
                ? (getChatState().messages[targetId] ?? [])
                      .filter((m) => m.seq !== optimisticSeq)
                      .filter(
                          (m) =>
                              m.kind === "user_msg" || m.kind === "model_turn",
                      )
                      .map((m) => ({
                          role: (m.kind === "user_msg"
                              ? "user"
                              : "assistant") as "user" | "assistant",
                          content: m.text ?? "",
                      }))
                : undefined;
            // Browser timezone — sourced once per send so the
            // server stamps the right zone into the per-turn
            // context. Detection is cheap and always available;
            // fall back to undefined on the off chance Intl
            // returns a falsy zone (older WebViews etc.).
            const browserTz =
                Intl.DateTimeFormat().resolvedOptions().timeZone || undefined;
            try {
                const resp = await postMessage(
                    targetId,
                    incognito
                        ? {
                              text,
                              incognito: true,
                              prior_messages: priorMessages,
                              timezone: browserTz,
                              attachments,
                              skill_names:
                                  skillNames.length > 0 ? skillNames : undefined,
                          }
                        : {
                              text,
                              timezone: browserTz,
                              attachments,
                              skill_names:
                                  skillNames.length > 0 ? skillNames : undefined,
                          },
                    getToken,
                );
                if (incognito) {
                    // Server emitted a `chat_message_outbound` on
                    // the WS bus with the final assistant text.
                    // That handler already clears the streaming
                    // buffer and appends to messages — we just
                    // append the assistant turn here as a fallback
                    // in case the WS race lost. The store dedupes
                    // on seq so a double-add is a no-op.
                    appendMessage(targetId, {
                        seq: optimisticSeq + 1,
                        kind: "model_turn",
                        text: resp.assistant_text ?? "",
                        actor: "agent",
                        committed_at: Math.floor(Date.now() / 1000),
                    });
                    clearStreamingBuffer(targetId);
                } else {
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
                    // sidebar without a manual reload. Skipped for
                    // incognito (nothing to title — the row doesn't
                    // exist).
                    if (isFirstTurn) {
                        void postGenerateTitle(targetId, getToken)
                            .then((r) => {
                                if (!r.skipped) {
                                    return listThreads(getToken).then(
                                        (tr) => {
                                            setThreads(tr.threads);
                                        },
                                    );
                                }
                            })
                            .catch((e) => {
                                console.warn("title generation failed", e);
                            });
                    }
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
                    // Streaming bubble takes over as the
                    // "agent is talking" affordance — clear the
                    // sticky tool-activity label so the
                    // indicator doesn't sit beside the bubble
                    // with stale text.
                    clearToolActivity(cid);
                }
                break;
            case "chat_message_outbound":
            case "chat_message_inbound":
                if (cid) {
                    clearStreamingBuffer(cid);
                    // Reply / inbound message lands → loader is
                    // unconditionally over for this turn.
                    clearToolActivity(cid);
                    // Incognito: nothing on the server to refetch.
                    // Skipping prevents the GET-then-empty-set-
                    // messages race that would clobber the local
                    // transcript and bounce ChatPane back to the
                    // welcome view (hasContent flips false → true →
                    // false within a frame).
                    if (!cid.startsWith("incognito:")) {
                        listThreads(getToken)
                            .then((r) => setThreads(r.threads))
                            .catch(() => {});
                        listMessages(cid, getToken)
                            .then((r) => setMessages(cid, r.messages))
                            .catch(() => {});
                    }
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
                        // Tool activity is meaningless once the
                        // agent has handed back to idle — clear the
                        // pill in case the matching `finished` pulse
                        // got dropped or arrived out-of-order.
                        clearToolActivity(cid);
                    }
                }
                break;
            case "agent_tool_activity":
                // Loader pulse — generated server-side when the
                // runner emits a tool-call request. Drives the
                // "Searching the web for X…" label inside the
                // message stream's activity row so the operator
                // knows what's happening during multi-round flows.
                //
                // 2026-05-02 — only `started` mutates the store.
                // Pre-fix `finished`/`failed` cleared the label
                // immediately, which felt like the line flashed
                // on/off between rounds — the operator saw a
                // bare spinner during the gap. The store now
                // holds the most-recent label until either the
                // next tool's `started` pulse replaces it or a
                // token-delta / message commit clears it via the
                // surrounding cases.
                if (
                    cid &&
                    typeof ev.tool_name === "string" &&
                    typeof ev.label === "string" &&
                    typeof ev.status === "string" &&
                    ev.status === "started"
                ) {
                    setToolActivity(cid, {
                        tool_name: ev.tool_name,
                        label: ev.label,
                    });
                }
                break;
            // ---- C4 — generic Card events (research, future
            // shell-session / file-pipeline tools all share this
            // surface). Project the WS payload into the card store;
            // MessageStream re-renders via the store's subscription.
            case "card_opened": {
                if (cid && typeof ev.card_id === "string") {
                    applyCardEvent(cid, {
                        kind: "card.opened",
                        committed_at:
                            typeof ev.committed_at === "number"
                                ? ev.committed_at
                                : Math.floor(Date.now() / 1000),
                        event_seq:
                            typeof ev.event_seq === "number"
                                ? ev.event_seq
                                : null,
                        payload: {
                            card_id: ev.card_id,
                            kind:
                                typeof ev.card_kind === "string"
                                    ? (ev.card_kind as
                                          | "long_running_task"
                                          | "research"
                                          | "shell_session"
                                          | "file_pipeline")
                                    : "long_running_task",
                            title: typeof ev.title === "string" ? ev.title : "",
                            summary:
                                typeof ev.summary === "string" ? ev.summary : "",
                            state:
                                typeof ev.state === "string"
                                    ? (ev.state as
                                          | "Pending"
                                          | "Running"
                                          | "Paused"
                                          | "Completed"
                                          | "Failed"
                                          | "Cancelled")
                                    : undefined,
                            details: ev.details,
                            actions: Array.isArray(ev.actions)
                                ? (ev.actions as never)
                                : [],
                        },
                    });
                }
                break;
            }
            case "card_progressed": {
                if (cid && typeof ev.card_id === "string") {
                    applyCardEvent(cid, {
                        kind: "card.progressed",
                        committed_at:
                            typeof ev.committed_at === "number"
                                ? ev.committed_at
                                : Math.floor(Date.now() / 1000),
                        event_seq:
                            typeof ev.event_seq === "number"
                                ? ev.event_seq
                                : null,
                        payload: {
                            card_id: ev.card_id,
                            state:
                                typeof ev.state === "string"
                                    ? (ev.state as
                                          | "Pending"
                                          | "Running"
                                          | "Paused"
                                          | "Completed"
                                          | "Failed"
                                          | "Cancelled")
                                    : undefined,
                            progress:
                                typeof ev.progress === "number"
                                    ? ev.progress
                                    : undefined,
                            phase:
                                typeof ev.phase === "string"
                                    ? ev.phase
                                    : undefined,
                            details: ev.details,
                            actions: Array.isArray(ev.actions)
                                ? (ev.actions as never)
                                : undefined,
                            summary:
                                typeof ev.summary === "string"
                                    ? ev.summary
                                    : undefined,
                        },
                    });
                }
                break;
            }
            case "card_closed": {
                if (cid && typeof ev.card_id === "string") {
                    applyCardEvent(cid, {
                        kind: "card.closed",
                        committed_at:
                            typeof ev.committed_at === "number"
                                ? ev.committed_at
                                : Math.floor(Date.now() / 1000),
                        event_seq:
                            typeof ev.event_seq === "number"
                                ? ev.event_seq
                                : null,
                        payload: {
                            card_id: ev.card_id,
                            state:
                                typeof ev.state === "string"
                                    ? (ev.state as
                                          | "Pending"
                                          | "Running"
                                          | "Paused"
                                          | "Completed"
                                          | "Failed"
                                          | "Cancelled")
                                    : "Completed",
                            summary:
                                typeof ev.summary === "string" ? ev.summary : "",
                            details: ev.details,
                            attachment_id:
                                typeof ev.attachment_id === "string"
                                    ? ev.attachment_id
                                    : undefined,
                            error:
                                typeof ev.error === "string"
                                    ? ev.error
                                    : undefined,
                        },
                    });
                }
                break;
            }
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

    // Keep the WsClient's onEvent indirection pointed at the latest
    // handler. See the long comment next to `handleWsEventRef` above
    // for why we can't just close over `handleWsEvent` directly.
    useEffect(() => {
        handleWsEventRef.current = handleWsEvent;
    }, [handleWsEvent]);

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
                <ErrorBanner
                    message={topError}
                    onDismiss={() => setTopError(null)}
                    className="mx-3 mt-3"
                    testId="chat-error-banner"
                />
                <ChatPane
                    activeId={activeId}
                    onSend={onSend}
                    getToken={getToken}
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
    getToken,
    onStop,
    incognito,
    onToggleIncognito,
    sendVoiceFrame,
    sendVoiceControl,
    voiceTranscript,
}: {
    activeId: string | null;
    onSend: (
        text: string,
        attachments: InlineAttachment[],
        skillNames: string[],
    ) => Promise<void> | void;
    getToken: () => string | null;
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
    // 2026-05-15 — probe the Standard backend's multimodal capability
    // so the Composer's `+` (attach photo) affordance only surfaces
    // when the loaded model actually accepts images. The hook is
    // safe to call here (always-on chat shell); it does one HTTP RTT
    // on mount and re-fetches when the token rotates.
    const caps = useBackendCapabilities(getToken, true);
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

    // 2026-04-28 — GSAP Flip transition WelcomeView → ActiveThreadPane
    // on first send. `captureBeforeFirstSend` gets called from
    // WelcomeView's onSend wrapper BEFORE the parent's onSend
    // triggers the state change that flips `hasContent`. The hook's
    // useLayoutEffect picks up the flip and animates the new
    // composer's appearance from the welcome composer's saved
    // position. Direct deep-link navigation (no welcome view ever
    // mounted) makes captureBeforeFirstSend a no-op so the hook
    // skips the timeline and the active pane mounts cleanly.
    //
    // After the timeline lands (or immediately under reduced-motion)
    // we focus the new composer's textarea so the operator can
    // keep typing — the welcome composer's textarea unmounts during
    // the swap, which would otherwise drop focus to <body>.
    const { captureBeforeFirstSend } = useChatTransition({
        hasContent,
        onComplete: () => {
            const ta = document.querySelector<HTMLTextAreaElement>(
                '[data-testid="composer-input"]',
            );
            ta?.focus();
        },
    });
    const onSendWithFlipCapture = useCallback(
        (
            text: string,
            attachments: InlineAttachment[],
            skillNames: string[],
        ) => {
            if (!hasContent) {
                captureBeforeFirstSend();
            }
            return onSend(text, attachments, skillNames);
        },
        [hasContent, captureBeforeFirstSend, onSend],
    );

    /// 2026-05-15 — `getSkills` for the composer's "Attach skill"
    /// menu item. Lazy: the Composer calls this the first time the
    /// picker opens, then caches locally for the rest of its
    /// instance. Re-fetches on every fresh Composer mount (so an
    /// admin who just authored a new skill in another tab sees it
    /// after a chat-pane remount). Re-uses the same admin route the
    /// Skills page hits since "controller == admin" today.
    const getSkills = useCallback(
        (): Promise<SkillListEntry[]> =>
            listSkills(getToken).then((r) => r.skills),
        [getToken],
    );
    // 2026-04-28 dev-only: diagnostic for the incognito flash bug.
    if (
        typeof activeId === "string" &&
        activeId.startsWith("incognito:")
    ) {
        // eslint-disable-next-line no-console
        console.log(
            "[ChatPane] render incognito",
            {
                activeId,
                messagesLen: messages?.length ?? null,
                streamingLen: streaming?.length ?? null,
                hasContent,
                pane: hasContent ? "ActiveThreadPane" : "WelcomeView",
            },
        );
    }

    if (!hasContent) {
        return (
            <>
                <VoiceStatusBar
                    transcript={voiceTranscript}
                    sendVoiceControl={sendVoiceControl}
                />
                <WelcomeView
                    onSend={onSendWithFlipCapture}
                    sendVoiceFrame={sendVoiceFrame}
                    sendVoiceControl={sendVoiceControl}
                    onStop={onStop}
                    busy={isSendingActive}
                    incognito={incognito}
                    onToggleIncognito={onToggleIncognito}
                    multimodal={caps.multimodal}
                    recommendedImageEdge={caps.recommendedImageEdge}
                    getSkills={getSkills}
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
                multimodal={caps.multimodal}
                recommendedImageEdge={caps.recommendedImageEdge}
                getSkills={getSkills}
            />
        </>
    );
}

function ActiveThreadPane({
    conversationId,
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
    multimodal,
    recommendedImageEdge,
    getSkills,
}: {
    conversationId: string;
    onSend: (
        text: string,
        attachments: InlineAttachment[],
        skillNames: string[],
    ) => Promise<void> | void;
    sendVoiceFrame: (bytes: ArrayBuffer) => boolean;
    sendVoiceControl: (payload: object) => boolean;
    multimodal?: boolean;
    recommendedImageEdge?: number;
    getSkills?: () => Promise<SkillListEntry[]>;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
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
        async (approvalId: string, verb: ApprovalVerb) => {
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

            <div className="execlaw-composer" data-flip-id="composer-shell">
                <ApprovalCard
                    approval={approval}
                    busy={approvalBusy}
                    onRespond={(id, verb) => void onApprovalRespond(id, verb)}
                />
                <Composer
                    onSend={onSend}
                    bridgedChannel={thread?.transport_channel ?? null}
                    sendVoiceFrame={sendVoiceFrame}
                    sendVoiceControl={sendVoiceControl}
                    voiceReadiness={voiceReadiness}
                    busy={isSending || (thread?.is_processing ?? false)}
                    multimodal={multimodal}
                    recommendedImageEdge={recommendedImageEdge}
                    getSkills={getSkills}
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

