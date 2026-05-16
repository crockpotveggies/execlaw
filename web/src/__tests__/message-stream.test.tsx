// Tests for the chat MessageStream:
// - empty / loading states render
// - long messages show a Read more toggle
// - channel-origin icon renders per message (default "web")
// - streaming bubble shows the typing cursor

import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MessageStream, stripSkillPrependBlock } from "../chat/MessageStream";
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

    /// 2026-04-28 — `web` (default origin) deliberately renders NO
    /// channel icon. The icon was visual noise for the common case;
    /// the test now pins the cleaner contract: web origin = no icon.
    it("does NOT render a channel-origin icon for default web messages", () => {
        setMessages("conv-1", [
            baseMsg(1, "hello"),
            baseMsg(2, "world", "model_turn"),
        ]);
        render(<MessageStream conversationId="conv-1" />);
        expect(screen.queryAllByTestId("channel-origin")).toHaveLength(0);
    });

    /// Non-web origins (Signal / email / voice / sms) DO surface the
    /// icon, including on user messages — even though web user
    /// messages drop the meta line entirely, an inbound Signal message
    /// keeps it so the operator can see which transport delivered it.
    it("respects an explicit channel_origin field on the payload", () => {
        // The store accepts arbitrary fields; the SPA reads channel_origin
        // off the message object opportunistically.
        appendMessage("conv-2", {
            ...baseMsg(1, "hi"),
            channel_origin: "signal",
        } as never);
        render(<MessageStream conversationId="conv-2" />);
        const origin = screen.getByTestId("channel-origin");
        // The new ChannelIcon component stamps `data-channel` so the
        // SPA tests pin transport-specific rendering by attribute.
        expect(origin).toHaveAttribute("data-channel", "signal");
        // Signal-origin messages must render the BRAND Signal logo
        // (svg), not a generic bi-* icon. The component picks an
        // <svg> element with the official Signal blue when the
        // channel is signal.
        expect(origin.tagName.toLowerCase()).toBe("svg");
    });

    it("uses bootstrap-icons for non-Signal transports", () => {
        // Email / voice / sms ride on `bi-*` since their generic
        // glyphs communicate the channel without needing a brand
        // SVG. The component renders a `<i class="bi …">` for these
        // — same DOM shape as before the Signal logo upgrade.
        appendMessage("conv-email", {
            ...baseMsg(1, "hi"),
            channel_origin: "email",
        } as never);
        render(<MessageStream conversationId="conv-email" />);
        const origin = screen.getByTestId("channel-origin");
        expect(origin).toHaveAttribute("data-channel", "email");
        expect(origin.tagName.toLowerCase()).toBe("i");
        expect(origin.className).toContain("bi-envelope");
    });

    /// 2026-04-28 — when the runner stamps `actor: "agent"` on a
    /// model_turn event, the meta line should read "agent" (not
    /// "agent · agent"). The collapse rule is: skip the actor suffix
    /// when it matches the role.
    it("collapses redundant 'agent · agent' on model_turn meta line", () => {
        setMessages("conv-3", [
            { ...baseMsg(1, "hello world", "model_turn"), actor: "agent" },
        ]);
        render(<MessageStream conversationId="conv-3" />);
        const stream = screen.getByTestId("message-stream");
        // Exactly one "agent" — no duplicate.
        const matches = stream.textContent?.match(/agent/g) ?? [];
        expect(matches.length).toBe(1);
        expect(stream.textContent).not.toContain("agent · agent");
    });

    /// 2026-04-28 — web-origin user messages render their bubble
    /// alone, no meta line. The right-aligned pill is the only
    /// affordance needed.
    it("hides the meta line entirely for web-origin user messages", () => {
        setMessages("conv-4", [baseMsg(1, "hello there")]);
        const view = render(<MessageStream conversationId="conv-4" />);
        // Meta div is `.execlaw-msg__meta`; it should not exist for a
        // plain web user message.
        expect(
            view.container.querySelector(".execlaw-msg__meta"),
        ).toBeNull();
        // The bubble itself is still rendered (with the user's
        // text + the is-user modifier).
        expect(
            view.container.querySelector(".execlaw-msg__bubble.is-user"),
        ).toBeTruthy();
    });

    it("renders long messages in full — no clamp, no Read-more affordance", () => {
        // The Phase-6 truncation spec was reverted 2026-05-15. Long
        // agent replies and drafted long-form content are exactly what
        // the operator wants to read inline; hiding them behind a
        // click was friction. Pin the new contract: the full text
        // reaches the DOM and no Read-more button is ever rendered.
        const longText = Array.from(
            { length: 30 },
            (_, i) => `line-${i + 1}`,
        ).join("\n");
        setMessages("conv-long", [baseMsg(1, longText)]);
        render(<MessageStream conversationId="conv-long" />);
        // No truncation marker / button.
        expect(screen.queryByTestId("msg-truncated")).toBeNull();
        expect(screen.queryByTestId("msg-read-more")).toBeNull();
        // Both ends of the text are present in the DOM — proves we're
        // not silently slicing.
        expect(screen.getByText(/line-1\b/)).toBeInTheDocument();
        expect(screen.getByText(/line-30\b/)).toBeInTheDocument();
    });

    it("doesn't render Read more on short messages either", () => {
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

    // ---- scroll-to-bottom button (2026-04-28) -----------------------
    // jsdom doesn't run layout, so `scrollHeight` / `clientHeight`
    // are 0 by default. We patch them per-test to simulate the
    // operator's scroll position and drive the floating ↓ button's
    // visibility + click contract.

    /// Small helper: stamp scroll-geometry properties on a node so
    /// the component's `onScroll` math reflects the simulated layout.
    function setScroll(
        el: HTMLElement,
        { scrollTop, scrollHeight, clientHeight }: {
            scrollTop: number;
            scrollHeight: number;
            clientHeight: number;
        },
    ) {
        // `writable: true` so the component's autoscroll path
        // (`el.scrollTop = el.scrollHeight`) can still run without
        // tripping the read-only property error in jsdom.
        Object.defineProperty(el, "scrollTop", {
            configurable: true,
            writable: true,
            value: scrollTop,
        });
        Object.defineProperty(el, "scrollHeight", {
            configurable: true,
            writable: true,
            value: scrollHeight,
        });
        Object.defineProperty(el, "clientHeight", {
            configurable: true,
            writable: true,
            value: clientHeight,
        });
    }

    it("doesn't show the ↓ button when the operator is at the bottom", () => {
        setMessages("conv-bot", [
            baseMsg(1, "hello"),
            baseMsg(2, "world", "model_turn"),
        ]);
        render(<MessageStream conversationId="conv-bot" />);
        // Initial state: at-bottom — button hidden.
        expect(screen.queryByTestId("scroll-to-bottom")).toBeNull();
    });

    it("surfaces the ↓ button after the operator scrolls up", () => {
        setMessages("conv-up", [
            baseMsg(1, "first"),
            baseMsg(2, "second", "model_turn"),
        ]);
        render(<MessageStream conversationId="conv-up" />);
        const stream = screen.getByTestId("message-stream");
        // Simulate "scrolled up by 200 px from a 1000-px tall content
        // window inside a 400-px viewport". distanceFromBottom = 400.
        setScroll(stream, {
            scrollTop: 200,
            scrollHeight: 1000,
            clientHeight: 400,
        });
        fireEvent.scroll(stream);
        const btn = screen.getByTestId("scroll-to-bottom");
        expect(btn).toBeInTheDocument();
        expect(btn).toHaveAttribute("aria-label", "Scroll to latest message");
    });

    /// 2026-05-04 regression: cards rendered in MessageStream
    /// (research card, attachment chip, etc.) used to anchor to
    /// the outer scroll surface and run the full viewport width —
    /// out of alignment with the surrounding chat bubbles. Each
    /// card is now wrapped in `.execlaw-msg .execlaw-msg--card`
    /// so it inherits the centered + clamped reading-column
    /// treatment messages get via MessageBubble.
    it("wraps each card in .execlaw-msg so it shares the chat-thread reading column", async () => {
        // Side-effect import the AttachmentCard renderer (its
        // module-level registerCardRenderer call is what wires
        // it into the registry). MessageStream's own imports
        // include the LongRunningTaskCard fallback but not the
        // per-kind ones — they're side-effect-imported by Chat.tsx
        // in production. Pulling AttachmentCard in here makes the
        // test self-contained.
        await import("../cards/AttachmentCard");
        const { applyCardEvent } = await import("../cards/cardStore");
        // Seed at least one message so MessageStream renders the
        // list (it short-circuits to a loading state when
        // `messages` is null). The card itself is what we're
        // asserting on — the message just keeps the stream live.
        setMessages("conv-card-margin", [baseMsg(1, "hi")]);
        // Open + close a tiny attachment card on the test
        // conversation so MessageStream renders it inline. Event
        // shapes mirror what the WS bus delivers (see
        // crates/server/src/events.rs).
        applyCardEvent("conv-card-margin", {
            kind: "card.opened",
            payload: {
                card_id: "card-1",
                kind: "attachment",
                title: "report.pdf",
                summary: "report.pdf (application/pdf)",
                state: "Running",
                details: {
                    attachment_id: "att-1",
                    filename: "report.pdf",
                    mime_type: "application/pdf",
                    download_url: "/api/attachments/att-1",
                },
                actions: [],
            },
            committed_at: 1,
            event_seq: 1,
        });
        applyCardEvent("conv-card-margin", {
            kind: "card.closed",
            payload: {
                card_id: "card-1",
                state: "Completed",
                summary: "report.pdf (application/pdf)",
                details: {
                    attachment_id: "att-1",
                    filename: "report.pdf",
                    mime_type: "application/pdf",
                    download_url: "/api/attachments/att-1",
                },
                attachment_id: "att-1",
                error: undefined,
            },
            committed_at: 2,
            event_seq: 2,
        });

        render(<MessageStream conversationId="conv-card-margin" />);
        const chip = screen.getByTestId("card-attachment");
        // Walk up to find the wrapper. Must be a direct (or near-
        // direct) ancestor with .execlaw-msg — otherwise the
        // shared margin/centering CSS doesn't apply.
        let cursor: HTMLElement | null = chip;
        let foundWrapper = false;
        while (cursor) {
            if (cursor.classList.contains("execlaw-msg")) {
                foundWrapper = true;
                break;
            }
            cursor = cursor.parentElement;
        }
        expect(foundWrapper).toBe(true);
    });

    it("clicking the ↓ button calls scrollTo and the button hides on next scroll event", () => {
        setMessages("conv-click", [
            baseMsg(1, "first"),
            baseMsg(2, "second", "model_turn"),
        ]);
        render(<MessageStream conversationId="conv-click" />);
        const stream = screen.getByTestId("message-stream");
        // Stub scrollTo so the click handler doesn't blow up in jsdom.
        const scrollToSpy = vi.fn();
        Object.defineProperty(stream, "scrollTo", {
            configurable: true,
            value: scrollToSpy,
        });
        // Scroll up far enough to surface the button.
        setScroll(stream, {
            scrollTop: 0,
            scrollHeight: 800,
            clientHeight: 400,
        });
        fireEvent.scroll(stream);
        const btn = screen.getByTestId("scroll-to-bottom");

        fireEvent.click(btn);
        expect(scrollToSpy).toHaveBeenCalledWith({
            top: 800,
            behavior: "smooth",
        });

        // After the click, simulate the resulting scroll landing at
        // bottom and emitting a scroll event. The button should
        // unmount.
        setScroll(stream, {
            scrollTop: 400,
            scrollHeight: 800,
            clientHeight: 400,
        });
        fireEvent.scroll(stream);
        expect(screen.queryByTestId("scroll-to-bottom")).toBeNull();
    });

    // ---- skill chip + prepend stripping (composer `+` menu) -----

    /// When the operator picked skills via the composer's `+` menu,
    /// the server prepends `<skill name="...">...</skill>` blocks
    /// onto the user_msg text. The bubble must:
    ///   * render only the original user text (blocks stripped), AND
    ///   * surface the applied skill names as a chip below the
    ///     bubble so the operator can see what shaped the response.
    it("strips the prepended skill block from the bubble and renders chips", () => {
        const prepended =
            '<skill name="test/foo">\n' +
            "Always answer in haiku.\n" +
            "</skill>\n\n" +
            "tell me a story";
        appendMessage("conv-skill", {
            ...baseMsg(1, prepended),
            applied_skill_names: ["test/foo"],
        } as never);
        render(<MessageStream conversationId="conv-skill" />);
        // Original user text reaches the bubble.
        expect(screen.getByText(/tell me a story/)).toBeInTheDocument();
        // Prepended block does NOT.
        expect(screen.queryByText(/Always answer in haiku/)).toBeNull();
        // Chip surfaces the applied skill name.
        const chips = screen.getAllByTestId("message-applied-skill");
        expect(chips).toHaveLength(1);
        expect(chips[0].getAttribute("data-skill-name")).toBe("test/foo");
    });

    /// Multi-skill: every leading block strips; chips for all names
    /// render in order.
    it("strips multiple consecutive skill blocks and renders chips in order", () => {
        const prepended =
            '<skill name="test/alpha">\n' +
            "alpha guidance\n" +
            "</skill>\n\n" +
            '<skill name="test/beta">\n' +
            "beta guidance\n" +
            "</skill>\n\n" +
            "do the thing";
        appendMessage("conv-multi", {
            ...baseMsg(1, prepended),
            applied_skill_names: ["test/alpha", "test/beta"],
        } as never);
        render(<MessageStream conversationId="conv-multi" />);
        expect(screen.getByText(/do the thing/)).toBeInTheDocument();
        expect(screen.queryByText(/alpha guidance/)).toBeNull();
        expect(screen.queryByText(/beta guidance/)).toBeNull();
        const chips = screen.getAllByTestId("message-applied-skill");
        expect(chips.map((c) => c.getAttribute("data-skill-name"))).toEqual([
            "test/alpha",
            "test/beta",
        ]);
    });

    /// Regression for "user typed text that looks like a skill block":
    /// without `applied_skill_names` set, the bubble renders the raw
    /// text verbatim. Stripping must be gated on the field being
    /// non-empty so we never trim user content.
    it("does NOT strip skill-shaped text when applied_skill_names is empty", () => {
        const literal =
            '<skill name="example/bad">should stay</skill>\n\nhi there';
        setMessages("conv-no-skills", [baseMsg(1, literal)]);
        render(<MessageStream conversationId="conv-no-skills" />);
        // The literal user text is preserved (markdown renders the
        // angle brackets as literal text since no html mode is on).
        const stream = screen.getByTestId("message-stream");
        expect(stream.textContent).toContain("should stay");
        expect(screen.queryByTestId("message-applied-skills")).toBeNull();
    });
});

describe("stripSkillPrependBlock", () => {
    it("returns the original text unchanged when no skill block is present", () => {
        expect(stripSkillPrependBlock("hello world")).toBe("hello world");
    });

    it("strips a single block plus the blank line separator", () => {
        const t = '<skill name="ns/x">\nbody\n</skill>\n\nuser text';
        expect(stripSkillPrependBlock(t)).toBe("user text");
    });

    it("strips multiple consecutive blocks", () => {
        const t =
            '<skill name="ns/a">a</skill>\n\n' +
            '<skill name="ns/b">b</skill>\n\n' +
            "go";
        expect(stripSkillPrependBlock(t)).toBe("go");
    });

    it("only strips leading blocks, not blocks embedded in user text", () => {
        const t = 'plain text\n\n<skill name="ns/x">should stay</skill>';
        // No leading block → nothing stripped.
        expect(stripSkillPrependBlock(t)).toBe(t);
    });
});
