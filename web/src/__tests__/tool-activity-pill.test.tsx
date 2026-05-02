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

    it("shows the label with a spinner once an activity is set", () => {
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
        expect(pill.textContent ?? "").toContain("🔎");
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

    it("renders a generic title for tools with no curated icon entry", () => {
        render(<ToolActivityPill conversationId="c-fallback" />);
        act(() => {
            setToolActivity("c-fallback", {
                tool_name: "frobnicate_widget",
                label: "Frobnicate widget",
            });
        });
        const pill = screen.getByTestId("tool-activity-pill");
        expect(pill.textContent ?? "").toContain("Frobnicate widget");
    });
});
