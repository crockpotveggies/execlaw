import { afterEach, describe, expect, it } from "vitest";
import {
    __resetChatStore,
    appendMessage,
    appendStreamingToken,
    clearStreamingBuffer,
    getChatState,
    markUnread,
    setActiveThread,
    setMessages,
    setThreads,
} from "../chat/store";
import type { MessageView, ThreadSummary } from "../api/endpoints";

const T = (id: string, overrides: Partial<ThreadSummary> = {}): ThreadSummary => ({
    conversation_id: id,
    kind: "ControllerDM",
    phase: "idle",
    trust_class: "Controller",
    modality: "Text",
    display_name: null,
    is_pinned: false,
    is_ephemeral: false,
    ephemeral_expires_at: null,
    last_seq: 0,
    ...overrides,
});

const M = (seq: number, text: string): MessageView => ({
    seq,
    kind: "user_msg",
    text,
    actor: "user",
    committed_at: 0,
});

describe("chat store", () => {
    afterEach(() => {
        __resetChatStore();
    });

    it("setThreads replaces the list", () => {
        setThreads([T("a"), T("b")]);
        expect(getChatState().threads.map((t) => t.conversation_id)).toEqual([
            "a",
            "b",
        ]);
    });

    it("setThreads preserves local UI flags across refreshes", () => {
        setThreads([T("a")]);
        markUnread("a");
        setThreads([T("a", { last_seq: 5 })]);
        const t = getChatState().threads[0];
        expect(t.last_seq).toBe(5);
        expect(t.has_unread).toBe(true);
    });

    it("setActiveThread clears unread on the activated thread", () => {
        setThreads([T("a"), T("b")]);
        markUnread("a");
        markUnread("b");
        setActiveThread("a");
        const after = getChatState().threads;
        expect(after.find((t) => t.conversation_id === "a")!.has_unread).toBe(
            false,
        );
        // Other threads keep their unread state.
        expect(after.find((t) => t.conversation_id === "b")!.has_unread).toBe(
            true,
        );
    });

    it("appendMessage is idempotent on duplicate seq", () => {
        appendMessage("conv", M(1, "first"));
        appendMessage("conv", M(1, "first")); // dup
        appendMessage("conv", M(2, "second"));
        expect(getChatState().messages.conv.map((m) => m.seq)).toEqual([1, 2]);
    });

    it("appendMessage keeps messages sorted by seq", () => {
        appendMessage("conv", M(3, "c"));
        appendMessage("conv", M(1, "a"));
        appendMessage("conv", M(2, "b"));
        expect(
            getChatState().messages.conv.map((m) => m.text),
        ).toEqual(["a", "b", "c"]);
    });

    it("setMessages replaces the entire history for a conversation", () => {
        appendMessage("conv", M(1, "first"));
        setMessages("conv", [M(10, "later")]);
        expect(getChatState().messages.conv.map((m) => m.seq)).toEqual([10]);
    });

    it("appendStreamingToken concatenates and toggles is_thinking", () => {
        setThreads([T("conv")]);
        appendStreamingToken("conv", "Hel");
        appendStreamingToken("conv", "lo");
        expect(getChatState().streamingBuffer.conv).toBe("Hello");
        expect(
            getChatState().threads.find((t) => t.conversation_id === "conv")!
                .is_thinking,
        ).toBe(true);
    });

    it("clearStreamingBuffer drops the buffer and resets is_thinking", () => {
        setThreads([T("conv")]);
        appendStreamingToken("conv", "abc");
        clearStreamingBuffer("conv");
        expect(getChatState().streamingBuffer.conv).toBeUndefined();
        expect(
            getChatState().threads.find((t) => t.conversation_id === "conv")!
                .is_thinking,
        ).toBe(false);
    });
});
