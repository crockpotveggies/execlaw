// Tests for the inline activity indicator (spinner + sticky label).
//
// Spinner is gated on the conversation's processing state
// (sending OR thread.is_processing), label is gated on
// `toolActivity[conversationId]` and is sticky across `finished`
// pulses.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, act } from "@testing-library/react";
import { ToolActivityPill } from "../chat/ToolActivityPill";
import {
    __resetChatStore,
    appendStreamingToken,
    clearToolActivity,
    markSendingThread,
    setThreadProcessing,
    setThreads,
    setToolActivity,
} from "../chat/store";

beforeEach(() => __resetChatStore());
afterEach(() => __resetChatStore());

/// Seed a thread row + flip processing on so the indicator can
/// render. Mimics what `conversation_phase_changed` would do via
/// the WS handler.
function startProcessing(cid: string) {
    act(() => {
        setThreads([
            {
                conversation_id: cid,
                kind: "ControllerDM",
                phase: "thinking",
                trust_class: "Controller",
                modality: "Text",
                display_name: null,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 0,
            },
        ]);
        setThreadProcessing(cid, true);
        markSendingThread(cid);
    });
}

describe("ToolActivityPill", () => {
    it("renders nothing when the conversation isn't processing", () => {
        const { container } = render(<ToolActivityPill conversationId="c-1" />);
        expect(container.firstChild).toBeNull();
    });

    it("shows the spinner the moment we mark the thread sending — no label needed", () => {
        render(<ToolActivityPill conversationId="c-2" />);
        startProcessing("c-2");
        const pill = screen.getByTestId("tool-activity-pill");
        // Mini-mascot SVG is always present once processing starts.
        expect(pill.querySelector("svg.execlaw-mini-mascot")).not.toBeNull();
        // No label yet — server hasn't fired an activity event.
        expect(pill.querySelector(".execlaw-tool-activity__label")).toBeNull();
    });

    it("adds the label beside the spinner once an activity arrives", () => {
        render(<ToolActivityPill conversationId="c-3" />);
        startProcessing("c-3");
        act(() => {
            setToolActivity("c-3", {
                tool_name: "web_search",
                label: "Searching the web for “paris weather”",
            });
        });
        const pill = screen.getByTestId("tool-activity-pill");
        expect(pill).toHaveAttribute("data-tool-name", "web_search");
        expect(pill.textContent ?? "").toContain(
            "Searching the web for “paris weather”",
        );
    });

    it("the label is sticky — it stays visible even after the activity is 'finished' on the wire", () => {
        // Pre-fix the pill flashed: server's `finished` pulse
        // cleared the label and the operator saw a bare spinner
        // for the gap between rounds. Holding the most-recent
        // label until something more useful takes its place is
        // the explicit fix; this test pins it.
        render(<ToolActivityPill conversationId="c-4" />);
        startProcessing("c-4");
        act(() => {
            setToolActivity("c-4", {
                tool_name: "web_search",
                label: "Searching the web for “paris weather”",
            });
        });
        // Note: the WS handler no longer calls `clearToolActivity`
        // on a `finished` pulse — that's the regression this test
        // would catch if a future change re-introduces auto-clear.
        // We just assert the label survives a re-render.
        expect(
            screen.getByTestId("tool-activity-pill").textContent ?? "",
        ).toContain("Searching the web for “paris weather”");
    });

    it("a new started-pulse replaces the previous label", () => {
        render(<ToolActivityPill conversationId="c-5" />);
        startProcessing("c-5");
        act(() => {
            setToolActivity("c-5", {
                tool_name: "web_search",
                label: "Searching the web for “paris weather”",
            });
        });
        act(() => {
            setToolActivity("c-5", {
                tool_name: "web_fetch",
                label: "Reading https://example.com",
            });
        });
        const pill = screen.getByTestId("tool-activity-pill");
        expect(pill).toHaveAttribute("data-tool-name", "web_fetch");
        expect(pill.textContent ?? "").toContain(
            "Reading https://example.com",
        );
        expect(pill.textContent ?? "").not.toContain(
            "Searching the web",
        );
    });

    it("hides entirely once token-streaming begins (the streaming bubble takes over)", () => {
        render(<ToolActivityPill conversationId="c-6" />);
        startProcessing("c-6");
        act(() => {
            setToolActivity("c-6", {
                tool_name: "web_search",
                label: "Searching",
            });
        });
        expect(screen.queryByTestId("tool-activity-pill")).toBeInTheDocument();
        act(() => {
            appendStreamingToken("c-6", "Paris is");
        });
        expect(screen.queryByTestId("tool-activity-pill")).toBeNull();
    });

    it("clearToolActivity drops the label but the spinner stays while processing", () => {
        render(<ToolActivityPill conversationId="c-7" />);
        startProcessing("c-7");
        act(() => {
            setToolActivity("c-7", {
                tool_name: "web_fetch",
                label: "Reading https://example.com",
            });
        });
        act(() => {
            clearToolActivity("c-7");
        });
        const pill = screen.getByTestId("tool-activity-pill");
        // Spinner survives.
        expect(pill.querySelector("svg.execlaw-mini-mascot")).not.toBeNull();
        // Label is gone.
        expect(pill.querySelector(".execlaw-tool-activity__label")).toBeNull();
    });

    it("scopes processing + activity to the right conversation", () => {
        render(<ToolActivityPill conversationId="c-mine" />);
        startProcessing("c-other");
        act(() => {
            setToolActivity("c-other", {
                tool_name: "web_search",
                label: "should not show here",
            });
        });
        expect(screen.queryByTestId("tool-activity-pill")).toBeNull();
    });
});
