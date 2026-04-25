import { afterEach, describe, expect, it } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { Sidebar } from "../chat/Sidebar";
import {
    __resetChatStore,
    appendStreamingToken,
    markUnread,
    setThreads,
    getChatState,
} from "../chat/store";
import { AuthProvider } from "../auth/AuthContext";

afterEach(() => __resetChatStore());

function rerender(ui: React.ReactElement) {
    return render(<AuthProvider>{ui}</AuthProvider>);
}

describe("Sidebar", () => {
    it("shows the empty hint when there are no threads", () => {
        rerender(<Sidebar onNewThread={() => {}} />);
        expect(screen.getByText(/no threads yet/i)).toBeInTheDocument();
    });

    it("renders one thread item per store row, in store order", () => {
        setThreads([
            {
                conversation_id: "controller-thread:c1",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: null,
                is_pinned: true,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 0,
            },
            {
                conversation_id: "conv-bbb",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Q4 plans",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 7,
            },
            {
                conversation_id: "abcd1234-rest-of-uuid",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: null,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 1,
            },
        ]);
        rerender(<Sidebar onNewThread={() => {}} />);
        const items = screen.getAllByTestId("sidebar-thread");
        expect(items).toHaveLength(3);
        expect(items[0]).toHaveAttribute(
            "data-thread-id",
            "controller-thread:c1",
        );
        // Controller-thread fallback label.
        expect(items[0]).toHaveTextContent("Control thread");
        // display_name wins for the second.
        expect(items[1]).toHaveTextContent("Q4 plans");
        // Fallback label for an unnamed non-controller thread.
        expect(items[2]).toHaveTextContent("New chat · abcd12");
    });

    it("clicking a thread sets it active in the store", () => {
        setThreads([
            {
                conversation_id: "conv-x",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: null,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 0,
            },
        ]);
        rerender(<Sidebar onNewThread={() => {}} />);
        fireEvent.click(screen.getByTestId("sidebar-thread"));
        expect(getChatState().activeId).toBe("conv-x");
    });

    it("renders an unread dot, then a spinner while thinking", () => {
        setThreads([
            {
                conversation_id: "conv-unread",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Unread one",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 0,
            },
        ]);
        markUnread("conv-unread");
        const view = rerender(<Sidebar onNewThread={() => {}} />);
        expect(
            view.container.querySelector(".execlaw-thread-dot.is-unread"),
        ).toBeTruthy();
        // Toggle thinking → spinner replaces dot.
        act(() => {
            appendStreamingToken("conv-unread", "tok");
        });
        expect(view.container.querySelector(".execlaw-thread-spinner")).toBeTruthy();
    });

    it("new-chat button fires the callback", () => {
        let count = 0;
        rerender(<Sidebar onNewThread={() => count++} />);
        fireEvent.click(screen.getByTestId("sidebar-new-thread"));
        expect(count).toBe(1);
    });
});
