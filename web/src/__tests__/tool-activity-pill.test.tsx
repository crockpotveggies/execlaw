// Smoke tests for the loader pill that surfaces per-tool agent
// activity ("Searching the web for X…").

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, act } from "@testing-library/react";
import { ToolActivityPill } from "../chat/ToolActivityPill";
import {
    __resetChatStore,
    clearToolActivity,
    setToolActivity,
} from "../chat/store";

beforeEach(() => __resetChatStore());
afterEach(() => __resetChatStore());

describe("ToolActivityPill", () => {
    it("renders nothing when there is no activity for the conversation", () => {
        const { container } = render(<ToolActivityPill conversationId="c-1" />);
        expect(container.firstChild).toBeNull();
    });

    it("shows the label and the mini-mascot SVG once an activity is set", () => {
        render(<ToolActivityPill conversationId="c-2" />);
        act(() => {
            setToolActivity("c-2", {
                tool_name: "web_search",
                label: "Searching the web for “paris weather”",
            });
        });
        const pill = screen.getByTestId("tool-activity-pill");
        expect(pill).toHaveAttribute("data-tool-name", "web_search");
        expect(pill.textContent ?? "").toContain(
            "Searching the web for “paris weather”",
        );
        // Mini mascot is the spinner now (replaces the old emoji
        // icon + CSS ring). Asserting on the SVG class hooks
        // catches accidental regressions of the structure that
        // the rotor + iris-tracking logic depend on.
        const svg = pill.querySelector("svg.execlaw-mini-mascot");
        expect(svg).not.toBeNull();
        expect(svg?.querySelector(".execlaw-mini-mascot__rotor")).not.toBeNull();
        expect(svg?.querySelector(".execlaw-mini-mascot__iris")).not.toBeNull();
    });

    it("disappears once the activity is cleared", () => {
        render(<ToolActivityPill conversationId="c-3" />);
        act(() => {
            setToolActivity("c-3", {
                tool_name: "web_fetch",
                label: "Reading https://example.com",
            });
        });
        expect(screen.getByTestId("tool-activity-pill")).toBeInTheDocument();
        act(() => {
            clearToolActivity("c-3");
        });
        expect(screen.queryByTestId("tool-activity-pill")).toBeNull();
    });

    it("scopes activity to the right conversation (others stay clean)", () => {
        render(<ToolActivityPill conversationId="c-mine" />);
        act(() => {
            setToolActivity("c-other", {
                tool_name: "web_search",
                label: "should not show here",
            });
        });
        expect(screen.queryByTestId("tool-activity-pill")).toBeNull();
    });

    it("renders a generic label for tools with no curated entry — mascot still spins", () => {
        render(<ToolActivityPill conversationId="c-fallback" />);
        act(() => {
            setToolActivity("c-fallback", {
                tool_name: "frobnicate_widget",
                label: "Frobnicate widget",
            });
        });
        const pill = screen.getByTestId("tool-activity-pill");
        expect(pill.textContent ?? "").toContain("Frobnicate widget");
        // Mascot is independent of the tool family — every
        // activity gets the same spinner, only the label changes.
        expect(pill.querySelector("svg.execlaw-mini-mascot")).not.toBeNull();
    });
});
