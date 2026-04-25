// Vertically scrolling message list for the active thread.
//
// Auto-scrolls to the latest message on append. Long messages get a
// fixed-height clamp + "Read more…" affordance per the Phase-6
// truncation spec, but for now we keep the markup minimal and let the
// truncation pass land in 6c when copy-pasted documents start showing
// up in test runs.

import { useEffect, useRef } from "react";
import type { MessageView } from "../api/endpoints";
import { useChatState } from "./store";

interface Props {
    conversationId: string;
}

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
                    <div className="execlaw-msg__meta">agent · streaming</div>
                    <div className="execlaw-msg__bubble">{streaming}</div>
                </div>
            )}
        </div>
    );
}

function MessageBubble({ message }: { message: MessageView }) {
    const role = roleFor(message);
    return (
        <div className="execlaw-msg">
            <div className="execlaw-msg__meta">
                {role}
                {message.actor ? ` · ${message.actor}` : ""}
            </div>
            <div
                className={
                    "execlaw-msg__bubble" +
                    (message.kind === "user_msg" ? " is-user" : "") +
                    (isToolKind(message.kind) ? " is-tool" : "")
                }
            >
                {message.text ?? renderToolFallback(message)}
            </div>
        </div>
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
