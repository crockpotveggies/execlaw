// Vertically scrolling message list for the active thread.
//
// Auto-scrolls to the latest message on append IF the operator is
// already at the bottom. When the operator has scrolled up to read
// history, incoming messages no longer yank the viewport — instead
// a small ↓ button surfaces above the composer; clicking it snaps
// to the latest. Long messages get a fixed-height clamp +
// "Read more…" affordance per the Phase-6 truncation spec. Each
// bubble also renders a subtle channel-origin icon (signal / email
// / voice) so the controller can see at a glance which transport
// delivered the message.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MessageView } from "../api/endpoints";
import { getCardRenderer } from "../cards/CardRenderer";
import { useCardsForConversation } from "../cards/cardStore";
import type { Card } from "../cards/types";
import { MarkdownContent } from "../components/MarkdownContent";
import { ToolActivityPill } from "./ToolActivityPill";
import { useChatState } from "./store";

interface Props {
    conversationId: string;
}

/** Lines past which the bubble truncates with a "Read more…" toggle. */
const TRUNCATE_LINES = 12;
/** Char heuristic — fall back to length when the line count is hard to gauge. */
const TRUNCATE_CHARS = 1200;

/** Pixel slack on the at-bottom check. Browsers can leave a fractional
 *  gap between scrollTop+clientHeight and scrollHeight even when the
 *  user is visually at the floor; 8 px swallows that noise. */
const AT_BOTTOM_SLACK_PX = 8;

/// Discriminated union the chronological-render loop walks. Cards
/// and messages are interleaved by their wall-clock timestamp.
type StreamItem =
    | { kind: "message"; message: MessageView; sortKey: number }
    | { kind: "card"; card: Card; sortKey: number };

export function MessageStream({ conversationId }: Props) {
    const messages = useChatState(
        (s) => s.messages[conversationId] ?? null,
    );
    const streaming = useChatState(
        (s) => s.streamingBuffer[conversationId] ?? null,
    );
    const cards = useCardsForConversation(conversationId);
    const items: StreamItem[] = useMemo(() => {
        const acc: StreamItem[] = [];
        if (messages) {
            for (const m of messages) {
                acc.push({ kind: "message", message: m, sortKey: m.seq });
            }
        }
        // 2026-05-03 — cards now arrive with their real
        // `state_events.seq` from the WS payload (`event_seq`).
        // Use it as the sortKey so cards land inline at their
        // true position in the chat-thread timeline, alongside
        // messages, instead of being pinned to the tail.
        //
        // Fallback for legacy WS payloads / fixtures with no seq:
        // synthesise a key from `opened_at` × 1e6 so the card
        // still lands chronologically (and at the bottom for
        // `event_seq`-less cards opened in the past, matching
        // the prior behavior).
        for (const c of cards) {
            const sortKey =
                c.event_seq !== null && c.event_seq !== undefined
                    ? c.event_seq
                    : c.opened_at * 1_000_000;
            acc.push({
                kind: "card",
                card: c,
                sortKey,
            });
        }
        acc.sort((a, b) => a.sortKey - b.sortKey);
        return acc;
    }, [messages, cards]);
    const scrollRef = useRef<HTMLDivElement | null>(null);
    const [isAtBottom, setIsAtBottom] = useState(true);

    // Auto-stick to the bottom only when the operator is already
    // there. Mid-history scroll-up means "I'm reading older content,
    // don't yank me away" — incoming messages just pile under the
    // viewport and the ↓ button surfaces.
    useEffect(() => {
        const el = scrollRef.current;
        if (!el) return;
        if (isAtBottom) {
            el.scrollTop = el.scrollHeight;
        }
    }, [messages, streaming, isAtBottom]);

    const onScroll = useCallback(() => {
        const el = scrollRef.current;
        if (!el) return;
        const distanceFromBottom =
            el.scrollHeight - el.scrollTop - el.clientHeight;
        setIsAtBottom(distanceFromBottom <= AT_BOTTOM_SLACK_PX);
    }, []);

    const scrollToBottom = useCallback(() => {
        const el = scrollRef.current;
        if (!el) return;
        el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    }, []);

    // Re-establish at-bottom on conversation switch so the new
    // thread starts in autoscroll mode.
    useEffect(() => {
        setIsAtBottom(true);
    }, [conversationId]);

    if (messages === null) {
        return (
            <div className="execlaw-stream-wrap">
                <div className="execlaw-stream" ref={scrollRef}>
                    <div className="execlaw-empty-state small">
                        Loading messages…
                    </div>
                </div>
            </div>
        );
    }

    if (messages.length === 0 && cards.length === 0 && !streaming) {
        return (
            <div className="execlaw-stream-wrap">
                <div className="execlaw-stream" ref={scrollRef}>
                    <div className="execlaw-empty-state">
                        <i
                            className="bi bi-chat-square-dots"
                            style={{
                                fontSize: "2rem",
                                display: "block",
                                marginBottom: "0.5rem",
                            }}
                            aria-hidden
                        />
                        No messages yet. Type below to start.
                    </div>
                </div>
            </div>
        );
    }

    return (
        <div className="execlaw-stream-wrap">
            <div
                className="execlaw-stream"
                ref={scrollRef}
                onScroll={onScroll}
                data-testid="message-stream"
            >
                {items.map((item) => {
                    if (item.kind === "message") {
                        const m = item.message;
                        return (
                            <MessageBubble
                                key={`msg-${m.kind}-${m.seq}`}
                                message={m}
                            />
                        );
                    }
                    const Renderer = getCardRenderer(item.card.kind);
                    return (
                        <Renderer
                            key={`card-${item.card.card_id}`}
                            card={item.card}
                        />
                    );
                })}
                {/* Live "what's the agent doing" pulse, mounted
                    inside the message stream where the next
                    assistant message will land. Shows nothing
                    when there's no active tool — only renders
                    while a `tool_call` round-trip is in flight. */}
                <ToolActivityPill conversationId={conversationId} />
                {streaming && (
                    <div className="execlaw-msg" data-testid="streaming-bubble">
                        <div className="execlaw-msg__meta">
                            agent · streaming
                            <span className="execlaw-streaming-cursor" aria-hidden>
                                ▍
                            </span>
                        </div>
                        <div className="execlaw-msg__bubble">
                            <MarkdownContent text={streaming} streaming />
                        </div>
                    </div>
                )}
            </div>
            {!isAtBottom && (
                <button
                    type="button"
                    className="execlaw-scroll-to-bottom"
                    onClick={scrollToBottom}
                    aria-label="Scroll to latest message"
                    data-testid="scroll-to-bottom"
                >
                    <i className="bi bi-arrow-down" aria-hidden />
                </button>
            )}
        </div>
    );
}

function MessageBubble({ message }: { message: MessageView }) {
    const role = roleFor(message);
    const text = message.text ?? renderToolFallback(message);
    const channelOrigin = readChannelOrigin(message);
    const lines = text.split("\n").length;
    const isLong = lines > TRUNCATE_LINES || text.length > TRUNCATE_CHARS;
    const [expanded, setExpanded] = useState(false);

    const display = isLong && !expanded ? clamp(text, TRUNCATE_LINES, TRUNCATE_CHARS) : text;

    // 2026-04-28 — meta-line cleanup:
    //   * Hide the channel-origin icon for the default `web` origin
    //     (adds visual noise; only useful when a non-web transport
    //     delivered the message — Signal / email / voice / sms).
    //   * Skip the `· {actor}` suffix when the actor is redundant
    //     with the role — "agent · agent" was the regression we
    //     caught here. The runner stamps `actor: "agent"` on every
    //     `model_turn` event; collapsing that into just "agent" reads
    //     cleaner.
    //   * Drop the meta entirely for the common case (web-origin
    //     user message). The right-aligned pill already telegraphs
    //     "you said this" — a "you" label adds no info. We DO keep
    //     the meta when a user message arrives via a non-web
    //     transport (Signal / email / voice / sms) so the operator
    //     can see "this came in over Signal" without inspecting the
    //     event payload.
    const showOriginIcon = channelOrigin !== "web";
    const actorSuffix =
        message.actor && message.actor !== role
            ? ` · ${message.actor}`
            : "";
    const isUserMessage = message.kind === "user_msg";
    const showMeta = !isUserMessage || channelOrigin !== "web";

    return (
        <div
            className={"execlaw-msg" + (isUserMessage ? " is-user" : "")}
        >
            {showMeta && (
                <div className="execlaw-msg__meta">
                    {showOriginIcon && (
                        <ChannelOriginIcon origin={channelOrigin} />
                    )}
                    {role}
                    {actorSuffix}
                </div>
            )}
            <div
                className={
                    "execlaw-msg__bubble" +
                    (isUserMessage ? " is-user" : "") +
                    (isToolKind(message.kind) ? " is-tool" : "") +
                    (isLong && !expanded ? " is-clamped" : "")
                }
                data-testid={isLong ? "msg-truncated" : undefined}
            >
                {/* Tool messages are JSON / monospace dumps — render
                    as raw text. Everything else (agent + user) goes
                    through the markdown pipeline so headings, lists,
                    code blocks, links, tables etc. render properly.
                    Markdown inside fenced code blocks is preserved
                    as literal text (remark's default). */}
                {isToolKind(message.kind) ? (
                    display
                ) : (
                    <MarkdownContent text={display} />
                )}
                {isLong && (
                    <div className="mt-2">
                        <button
                            type="button"
                            className="btn btn-link btn-sm p-0 execlaw-muted"
                            onClick={() => setExpanded((v) => !v)}
                            data-testid="msg-read-more"
                        >
                            {expanded ? "Show less" : "Read more…"}
                        </button>
                    </div>
                )}
            </div>
        </div>
    );
}

function clamp(text: string, lineLimit: number, charLimit: number): string {
    const lines = text.split("\n");
    let out = lines.slice(0, lineLimit).join("\n");
    if (out.length > charLimit) {
        out = out.slice(0, charLimit);
    }
    return out + "…";
}

function readChannelOrigin(m: MessageView): ChannelOrigin {
    // The server may attach a channel_origin to the event payload (§2.6).
    // We fall back to "web" when absent so the UI still renders an icon.
    const raw = (m as MessageView & { channel_origin?: unknown }).channel_origin;
    if (raw === "signal" || raw === "email" || raw === "voice" || raw === "sms") {
        return raw;
    }
    return "web";
}

type ChannelOrigin = "web" | "signal" | "email" | "voice" | "sms";

function ChannelOriginIcon({ origin }: { origin: ChannelOrigin }) {
    const icon = (
        {
            web: "bi-globe",
            signal: "bi-chat-dots",
            email: "bi-envelope",
            voice: "bi-mic",
            sms: "bi-phone",
        } as const
    )[origin];
    return (
        <i
            className={`bi ${icon} execlaw-muted me-2`}
            aria-label={`channel: ${origin}`}
            data-testid="channel-origin"
            data-origin={origin}
        />
    );
}

function roleFor(m: MessageView): string {
    switch (m.kind) {
        case "user_msg":
            return "you";
        case "model_turn":
            return "agent";
        case "tool_use":
            return "tool · request";
        case "tool_result":
            return "tool · result";
        default:
            return m.kind;
    }
}

function isToolKind(kind: string): boolean {
    return kind === "tool_use" || kind === "tool_result";
}

function renderToolFallback(m: MessageView): string {
    return `[${m.kind} (no text payload)]`;
}
