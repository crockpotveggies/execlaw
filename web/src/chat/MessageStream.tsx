// Vertically scrolling message list for the active thread.
//
// Auto-scrolls to the latest message on append. Long messages get a
// fixed-height clamp + "Read more…" affordance per the Phase-6
// truncation spec. Each bubble also renders a subtle channel-origin
// icon (web / signal / email / voice) so the controller can see at a
// glance which transport delivered the message — required for the
// Control-thread merge UX (MIGRATION_PLAN §6).

import { useEffect, useRef, useState } from "react";
import type { MessageView } from "../api/endpoints";
import { useChatState } from "./store";

interface Props {
    conversationId: string;
}

/** Lines past which the bubble truncates with a "Read more…" toggle. */
const TRUNCATE_LINES = 12;
/** Char heuristic — fall back to length when the line count is hard to gauge. */
const TRUNCATE_CHARS = 1200;

export function MessageStream({ conversationId }: Props) {
    const messages = useChatState(
        (s) => s.messages[conversationId] ?? null,
    );
    const streaming = useChatState(
        (s) => s.streamingBuffer[conversationId] ?? null,
    );
    const scrollRef = useRef<HTMLDivElement | null>(null);

    useEffect(() => {
        const el = scrollRef.current;
        if (!el) return;
        el.scrollTop = el.scrollHeight;
    }, [messages, streaming]);

    if (messages === null) {
        return (
            <div className="execlaw-stream" ref={scrollRef}>
                <div className="execlaw-empty-state small">Loading messages…</div>
            </div>
        );
    }

    if (messages.length === 0 && !streaming) {
        return (
            <div className="execlaw-stream" ref={scrollRef}>
                <div className="execlaw-empty-state">
                    <i
                        className="bi bi-chat-square-dots"
                        style={{ fontSize: "2rem", display: "block", marginBottom: "0.5rem" }}
                        aria-hidden
                    />
                    No messages yet. Type below to start.
                </div>
            </div>
        );
    }

    return (
        <div
            className="execlaw-stream"
            ref={scrollRef}
            data-testid="message-stream"
        >
            {messages.map((m) => (
                <MessageBubble key={`${m.kind}-${m.seq}`} message={m} />
            ))}
            {streaming && (
                <div className="execlaw-msg" data-testid="streaming-bubble">
                    <div className="execlaw-msg__meta">
                        <ChannelOriginIcon origin="web" />
                        agent · streaming
                        <span className="execlaw-streaming-cursor" aria-hidden>
                            ▍
                        </span>
                    </div>
                    <div className="execlaw-msg__bubble">{streaming}</div>
                </div>
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

    return (
        <div className="execlaw-msg">
            <div className="execlaw-msg__meta">
                <ChannelOriginIcon origin={channelOrigin} />
                {role}
                {message.actor ? ` · ${message.actor}` : ""}
            </div>
            <div
                className={
                    "execlaw-msg__bubble" +
                    (message.kind === "user_msg" ? " is-user" : "") +
                    (isToolKind(message.kind) ? " is-tool" : "") +
                    (isLong && !expanded ? " is-clamped" : "")
                }
                data-testid={isLong ? "msg-truncated" : undefined}
            >
                {display}
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
