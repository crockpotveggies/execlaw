import { afterEach, describe, expect, it, vi } from "vitest";
import {
    act,
    fireEvent,
    render,
    screen,
    waitFor,
} from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { Sidebar } from "../chat/Sidebar";
import {
    __resetChatStore,
    appendStreamingToken,
    markUnread,
    setActiveThread,
    setThreads,
    getChatState,
} from "../chat/store";
import { AuthProvider } from "../auth/AuthContext";

afterEach(() => {
    __resetChatStore();
    vi.unstubAllGlobals();
    localStorage.clear();
});

function rerender(ui: React.ReactElement, initialPath = "/chat") {
    // Sidebar uses react-router's <Link> for the settings gear, so
    // tests must mount within a MemoryRouter. `initialPath` lets
    // route-aware behaviours (e.g. the .is-active gate on thread
    // rows) be exercised from the test.
    return render(
        <AuthProvider>
            <MemoryRouter initialEntries={[initialPath]}>{ui}</MemoryRouter>
        </AuthProvider>,
    );
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

    // ---- external-channel filter --------------------------------------

    it("hide-external toggle filters out non-controller-DM threads", () => {
        setThreads([
            // ControllerDM — stays visible.
            {
                conversation_id: "ctrl",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Solo chat",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 5,
            },
            // External — should be filtered when hide-external is on.
            {
                conversation_id: "ext-1",
                kind: "ExternalWithOutsider",
                phase: "idle",
                trust_class: "KnownLimited",
                modality: "Text",
                display_name: "Outsider",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 3,
            },
        ]);
        rerender(<Sidebar onNewThread={() => {}} />);
        // Toggle row only renders when at least one external thread exists.
        expect(
            screen.getByTestId("sidebar-external-toggle-row"),
        ).toBeInTheDocument();
        // Both threads visible by default.
        expect(screen.getAllByTestId("sidebar-thread")).toHaveLength(2);

        const toggle = screen.getByTestId(
            "sidebar-hide-external",
        ) as HTMLInputElement;
        fireEvent.click(toggle);
        expect(toggle.checked).toBe(true);
        const after = screen.getAllByTestId("sidebar-thread");
        expect(after).toHaveLength(1);
        expect(after[0]).toHaveAttribute("data-thread-id", "ctrl");
    });

    // ---- plugin UI panels under "More" --------------------------------

    it("expanding More reveals plugin UI panels when supplied", () => {
        const panels = [
            { plugin_id: "calendar", mount: "panels/calendar", entry: "ui.js" },
            { plugin_id: "search", mount: "panels/search", entry: "ui.js" },
        ];
        rerender(<Sidebar onNewThread={() => {}} uiPanels={panels} />);
        // Panels collapsed by default.
        expect(screen.queryAllByTestId("sidebar-panel")).toHaveLength(0);
        fireEvent.click(screen.getByTestId("sidebar-more-toggle"));
        const links = screen.getAllByTestId("sidebar-panel");
        expect(links).toHaveLength(2);
        expect(links[0]).toHaveAttribute("href", "/panels/calendar");
    });

    it("More section shows the empty hint when no panels are installed", () => {
        rerender(<Sidebar onNewThread={() => {}} uiPanels={[]} />);
        fireEvent.click(screen.getByTestId("sidebar-more-toggle"));
        expect(
            screen.getByText(/no plugin panels installed/i),
        ).toBeInTheDocument();
    });

    // ---- pinning regression tests (2026-04-28) -----------------------
    // The user reported "pinning appears to be broken and visually
    // does nothing." These tests pin down the contract end-to-end so a
    // future regression on the menu-click → PATCH → list-refresh →
    // re-render chain is caught immediately.

    it("clicking 'Pin to top' fires PATCH /api/chats/:id with is_pinned: true and re-fetches the list", async () => {
        localStorage.setItem("execlaw.access_token", "tok");
        localStorage.setItem("execlaw.refresh_token", "rtok");
        const calls: Array<{ url: string; method?: string; body?: unknown }> = [];
        const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
            calls.push({
                url,
                method: init?.method,
                body:
                    typeof init?.body === "string"
                        ? JSON.parse(init.body)
                        : undefined,
            });
            if (url === "/api/admin/me") {
                return new Response(
                    JSON.stringify({
                        user_id: "ctrl-1",
                        username: "ctrl",
                        display_name: "Ctrl",
                        email: null,
                        role: "controller",
                        last_login_at: null,
                    }),
                    { status: 200 },
                );
            }
            if (
                url === "/api/chats/conv-unpinned" &&
                init?.method === "PATCH"
            ) {
                return new Response(
                    JSON.stringify({
                        conversation_id: "conv-unpinned",
                        display_name: "Trip plan",
                        is_pinned: true,
                        is_ephemeral: false,
                        ephemeral_expires_at: null,
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/chats") {
                return new Response(
                    JSON.stringify({
                        threads: [
                            // After pin: server returns pinned-first.
                            {
                                conversation_id: "conv-unpinned",
                                kind: "ControllerDM",
                                phase: "idle",
                                trust_class: "Controller",
                                modality: "Text",
                                display_name: "Trip plan",
                                is_pinned: true,
                                is_ephemeral: false,
                                ephemeral_expires_at: null,
                                last_seq: 6,
                            },
                            {
                                conversation_id: "conv-recent",
                                kind: "ControllerDM",
                                phase: "idle",
                                trust_class: "Controller",
                                modality: "Text",
                                display_name: "Recent",
                                is_pinned: false,
                                is_ephemeral: false,
                                ephemeral_expires_at: null,
                                last_seq: 99,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        vi.stubGlobal("fetch", fetchMock);

        // Seed the store with pre-pin order: recent first, target second.
        setThreads([
            {
                conversation_id: "conv-recent",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Recent",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 99,
            },
            {
                conversation_id: "conv-unpinned",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Trip plan",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 6,
            },
        ]);
        rerender(<Sidebar onNewThread={() => {}} />);

        // Pre-click: target sits in second position.
        let rows = screen.getAllByTestId("sidebar-thread");
        expect(rows[1]).toHaveAttribute("data-thread-id", "conv-unpinned");

        // Open the per-row menu on the target.
        const menuButtons = screen.getAllByTestId("thread-row-menu-btn");
        // menuButtons[1] is the second row (conv-unpinned).
        await act(async () => {
            fireEvent.click(menuButtons[1]);
        });
        // Menu opened.
        expect(screen.getByTestId("thread-row-menu")).toBeInTheDocument();

        // Click Pin to top.
        await act(async () => {
            fireEvent.click(screen.getByTestId("thread-row-menu-pin"));
        });

        // Wait for the PATCH + list refresh to flush.
        await waitFor(() => {
            const patchCall = calls.find(
                (c) =>
                    c.url === "/api/chats/conv-unpinned" &&
                    c.method === "PATCH",
            );
            expect(patchCall).toBeTruthy();
            expect(patchCall?.body).toEqual({ is_pinned: true });
        });
        await waitFor(() => {
            expect(
                calls.some((c) => c.url === "/api/chats" && (c.method ?? "GET") === "GET"),
            ).toBe(true);
        });
        // After the refresh, the row order reflects the server's
        // is_pinned-DESC, last_seq-DESC ordering: pinned first, even
        // though it has a smaller last_seq than its sibling.
        await waitFor(() => {
            rows = screen.getAllByTestId("sidebar-thread");
            expect(rows[0]).toHaveAttribute(
                "data-thread-id",
                "conv-unpinned",
            );
            expect(rows[1]).toHaveAttribute("data-thread-id", "conv-recent");
        });
    });

    it("renders the pin icon for pinned rows and the dot for unpinned rows", () => {
        setThreads([
            {
                conversation_id: "pinned",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Pinned thread",
                is_pinned: true,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 1,
            },
            {
                conversation_id: "regular",
                kind: "ControllerDM",
                phase: "idle",
                trust_class: "Controller",
                modality: "Text",
                display_name: "Regular thread",
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: null,
                last_seq: 99,
            },
        ]);
        rerender(<Sidebar onNewThread={() => {}} />);
        const rows = screen.getAllByTestId("sidebar-thread");
        // Pinned row carries an icon with aria-label "Pinned".
        expect(rows[0].querySelector('[aria-label="Pinned"]')).toBeTruthy();
        expect(rows[0].querySelector(".bi-pin-angle-fill")).toBeTruthy();
        // Regular row renders the read/unread dot, NOT the pin icon.
        expect(rows[1].querySelector('[aria-label="Pinned"]')).toBeNull();
    });

    /// 2026-05-04 regression: navigating away from a chat thread
    /// to /research, /settings, etc. used to leave the
    /// `.is-active` highlight on the previously-viewed thread,
    /// reading as "you're viewing this thread right now" when
    /// the operator was actually on a different page. Sidebar now
    /// gates the highlight on the current pathname starting with
    /// `/chat`. The thread itself stays VISIBLE in the sidebar
    /// (so switch-back is one click) — just not highlighted.
    it("does not apply .is-active to threads when route is not /chat", () => {
        setThreads([
            {
                conversation_id: "conv-active-1",
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
        ]);
        setActiveThread("conv-active-1");

        // Mount on /research (any non-chat route works).
        rerender(<Sidebar onNewThread={() => {}} />, "/research");
        const rows = screen.getAllByTestId("sidebar-thread");
        expect(rows.length).toBeGreaterThan(0);
        // Active thread is still visible (single thread case),
        // but no row should carry .is-active.
        for (const row of rows) {
            expect(row.classList.contains("is-active")).toBe(false);
        }
    });

    it("applies .is-active when route IS /chat and the thread matches activeId", () => {
        setThreads([
            {
                conversation_id: "conv-active-2",
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
        ]);
        setActiveThread("conv-active-2");
        // Mount on /chat (the default).
        rerender(<Sidebar onNewThread={() => {}} />, "/chat");
        const row = screen.getByTestId("sidebar-thread");
        expect(row.classList.contains("is-active")).toBe(true);
    });
});
