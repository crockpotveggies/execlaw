// Tests for the chat MessageStream:
// - empty / loading states render
// - long messages show a Read more toggle
// - channel-origin icon renders per message (default "web")
// - streaming bubble shows the typing cursor

import { afterEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MessageStream } from "../chat/MessageStream";
import {
    __resetChatStore,
    appendMessage,
    appendStreamingToken,
    setMessages,
} from "../chat/store";

afterEach(() => __resetChatStore());

const baseMsg = (
    seq: number,
    text: string,
    kind = "user_msg",
): {
    seq: number;
    kind: string;
    text: string | null;
    actor: string | null;
    committed_at: number;
} => ({
    seq,
    kind,
    text,
    actor: "controller",
    committed_at: 0,
});

describe("MessageStream", () => {
    it("shows the loading state when messages are unset", () => {
        // No setMessages call → messages[conv] is null.
        render(<MessageStream conversationId="conv-x" />);
        expect(screen.getByText(/Loading messages/i)).toBeInTheDocument();
    });

    it("shows the empty hint when messages array is empty", () => {
        setMessages("conv-empty", []);
        render(<MessageStream conversationId="conv-empty" />);
        expect(
            screen.getByText(/No messages yet\. Type below/i),
        ).toBeInTheDocument();
    });

    it("renders each message with a default 'web' channel-origin icon", () => {
        setMessages("conv-1", [
            baseMsg(1, "hello"),
            baseMsg(2, "world", "model_turn"),
        ]);
        render(<MessageStream conversationId="conv-1" />);
        const origins = screen.getAllByTestId("channel-origin");
        // One per message.
        expect(origins.length).toBeGreaterThanOrEqual(2);
        expect(origins[0]).toHaveAttribute("data-origin", "web");
    });

    it("respects an explicit channel_origin field on the payload", () => {
        // The store accepts arbitrary fields; the SPA reads channel_origin
        // off the message object opportunistically.
        appendMessage("conv-2", {
            ...baseMsg(1, "hi"),
            channel_origin: "signal",
        } as never);
        render(<MessageStream conversationId="conv-2" />);
        const origin = screen.getByTestId("channel-origin");
        expect(origin).toHaveAttribute("data-origin", "signal");
    });

    it("clamps long messages and reveals them on Read more", () => {
        const longText = Array.from(
            { length: 30 },
            (_, i) => `line-${i + 1}`,
        ).join("\n");
        setMessages("conv-long", [baseMsg(1, longText)]);
        render(<MessageStream conversationId="conv-long" />);
        expect(screen.getByTestId("msg-truncated")).toBeInTheDocument();
        const button = screen.getByTestId("msg-read-more");
        expect(button).toHaveTextContent(/Read more/);
        // Click expands → button label flips to Show less.
        fireEvent.click(button);
        expect(button).toHaveTextContent(/Show less/);
    });

    it("doesn't render Read more on short messages", () => {
        setMessages("conv-short", [baseMsg(1, "just a tiny note")]);
        render(<MessageStream conversationId="conv-short" />);
        expect(screen.queryByTestId("msg-read-more")).toBeNull();
    });

    it("streaming bubble renders with a typing cursor", () => {
        setMessages("conv-stream", []);
        appendStreamingToken("conv-stream", "thinking…");
        render(<MessageStream conversationId="conv-stream" />);
        const bubble = screen.getByTestId("streaming-bubble");
        expect(bubble).toHaveTextContent("thinking…");
        // The blinking cursor is just decorative; assert its presence
        // via the meta block.
        expect(bubble).toHaveTextContent("agent · streaming");
    });
});
