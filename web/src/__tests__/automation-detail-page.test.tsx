// Tests for the /automations/:id detail page (M4b + M4c).
//
// ReactFlow does runtime DOM measurement; we mock `@xyflow/react`
// with a thin stand-in so the canvas test renders deterministically
// in jsdom without pulling the full layout engine into the test.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState as reactUseState } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { AutomationDetailPage } from "../settings/AutomationDetailPage";
import { AuthProvider } from "../auth/AuthContext";

// Mock ReactFlow at module level — the real lib needs ResizeObserver +
// SVG measurement which jsdom doesn't ship. Our canvas test only cares
// that the component renders a placeholder with the testid.
vi.mock("@xyflow/react", () => {
    // Tiny stand-ins for the `useNodesState` / `useEdgesState` hooks
    // — we don't exercise drag in jsdom, but the canvas imports them
    // unconditionally so the symbols need to exist.
    const useNodesState = (initial: unknown[]) => {
        const [s, set] = reactUseState(initial);
        return [s, set, () => {}] as const;
    };
    const useEdgesState = (initial: unknown[]) => {
        const [s, set] = reactUseState(initial);
        return [s, set, () => {}] as const;
    };
    return {
        ReactFlow: ({ children }: { children?: React.ReactNode }) => (
            <div data-testid="mock-reactflow">{children}</div>
        ),
        ReactFlowProvider: ({ children }: { children?: React.ReactNode }) => (
            <>{children}</>
        ),
        Background: () => null,
        Controls: () => null,
        MiniMap: () => null,
        Handle: () => null,
        Position: { Top: "top", Bottom: "bottom", Left: "left", Right: "right" },
        addEdge: (_c: unknown, edges: unknown[]) => edges,
        useReactFlow: () => ({
            screenToFlowPosition: ({ x, y }: { x: number; y: number }) => ({ x, y }),
        }),
        useNodesState,
        useEdgesState,
    };
});
vi.mock("@xyflow/react/dist/style.css", () => ({}));

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = () =>
    new Response(
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

const automationFixture = {
    id: "auto-1",
    name: "Ring watch",
    enabled: true,
    definition: {
        trigger: { kind: "webhook.received", when: null },
        nodes: [
            {
                id: "f1",
                kind: "Filter",
                config: { expr: "event.payload.zone == \"driveway\"" },
            },
            { id: "end", kind: "Terminal", config: {} },
        ],
        edges: [
            { from: "trigger", to: "f1", when: null },
            { from: "f1", to: "end", when: null },
        ],
    },
    created_at: 1_700_000_000,
    updated_at: 1_700_000_000,
};

function setupRoutedFetch(
    map: Array<[string, (url: string) => Response]>,
) {
    // Ordered list — first match wins. Tests must register the most-
    // specific path (e.g., `/runs`) before the broader `/automations/auto-1`
    // because both substrings appear in the same URL.
    fetchMock.mockImplementation((url: string | URL | Request, init?: RequestInit) => {
        void init;
        const s =
            typeof url === "string"
                ? url
                : url instanceof URL
                  ? url.toString()
                  : url.url;
        for (const [key, fn] of map) {
            if (s.includes(key)) return Promise.resolve(fn(s));
        }
        if (s.includes("/api/me")) return Promise.resolve(meResponse());
        return Promise.resolve(new Response(JSON.stringify({}), { status: 200 }));
    });
}

function mountAt(path: string) {
    return render(
        <MemoryRouter initialEntries={[path]}>
            <AuthProvider>
                <Routes>
                    <Route
                        path="/automations/:id"
                        element={<AutomationDetailPageRouted />}
                    />
                </Routes>
            </AuthProvider>
        </MemoryRouter>,
    );
}

// Mini wrapper that extracts the `id` from the URL like our routes/Automations.tsx does.
import { useParams } from "react-router-dom";
function AutomationDetailPageRouted() {
    const { id } = useParams<{ id: string }>();
    return <AutomationDetailPage id={id ?? "new"} />;
}

beforeEach(() => {
    localStorage.setItem("execlaw.access_token", "tok");
    localStorage.setItem("execlaw.refresh_token", "tok");
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("AutomationDetailPage (M4c view toggle + test-run)", () => {
    it("defaults to canvas view and renders the mocked ReactFlow placeholder", async () => {
        setupRoutedFetch([
            ["/runs", () => new Response(JSON.stringify([]), { status: 200 })],
            [
                "/automations/auto-1",
                () =>
                    new Response(JSON.stringify(automationFixture), { status: 200 }),
            ],
        ]);
        mountAt("/automations/auto-1");
        await waitFor(() => {
            expect(screen.getByTestId("automation-view-toggle")).toBeInTheDocument();
        });
        // Canvas testid wraps the mocked ReactFlow.
        expect(screen.getByTestId("automation-canvas")).toBeInTheDocument();
        expect(screen.getByTestId("mock-reactflow")).toBeInTheDocument();
        // Code textarea is NOT rendered when canvas is active.
        expect(screen.queryByTestId("automation-def-textarea")).toBeNull();
    });

    it("toggles to code view and shows the JSON textarea with the loaded definition", async () => {
        setupRoutedFetch([
            ["/runs", () => new Response(JSON.stringify([]), { status: 200 })],
            [
                "/automations/auto-1",
                () =>
                    new Response(JSON.stringify(automationFixture), { status: 200 }),
            ],
        ]);
        mountAt("/automations/auto-1");
        await waitFor(() => {
            expect(screen.getByTestId("automation-view-code")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("automation-view-code"));
        const ta = screen.getByTestId("automation-def-textarea") as HTMLTextAreaElement;
        expect(ta.value).toContain("webhook.received");
        expect(ta.value).toContain("driveway");
    });

    it("surfaces a parse-error pane when the JSON is malformed and view is canvas", async () => {
        setupRoutedFetch([
            ["/runs", () => new Response(JSON.stringify([]), { status: 200 })],
            [
                "/automations/auto-1",
                () =>
                    new Response(JSON.stringify(automationFixture), { status: 200 }),
            ],
        ]);
        mountAt("/automations/auto-1");
        await waitFor(() => {
            expect(screen.getByTestId("automation-view-toggle")).toBeInTheDocument();
        });
        // Switch to code, corrupt the JSON, switch back to canvas.
        fireEvent.click(screen.getByTestId("automation-view-code"));
        const ta = screen.getByTestId("automation-def-textarea") as HTMLTextAreaElement;
        fireEvent.change(ta, { target: { value: "{ not json" } });
        fireEvent.click(screen.getByTestId("automation-view-canvas"));
        expect(
            screen.getByTestId("automation-canvas-parse-error"),
        ).toBeInTheDocument();
    });

    it("test-run drawer fetches recent events and runs against the picked event", async () => {
        const recentEvents = [
            {
                id: "ev-1",
                kind: "webhook.received",
                source: "webhook:ring",
                received_at: 1_700_000_000_000,
                payload: { zone: "driveway" },
            },
        ];
        const dryRunResult = {
            outcome: "success",
            step_traces: [
                {
                    node_id: "f1",
                    input: {},
                    output: { passed: true },
                    ms: 1,
                    error: null,
                },
            ],
        };
        const callLog: string[] = [];
        setupRoutedFetch([
            [
                "/recent-events",
                () => {
                    callLog.push("recent");
                    return new Response(JSON.stringify(recentEvents), { status: 200 });
                },
            ],
            [
                "/test-run",
                () => {
                    callLog.push("test-run");
                    return new Response(JSON.stringify(dryRunResult), { status: 200 });
                },
            ],
            ["/runs", () => new Response(JSON.stringify([]), { status: 200 })],
            [
                "/automations/auto-1",
                () => {
                    callLog.push("get-auto");
                    return new Response(JSON.stringify(automationFixture), {
                        status: 200,
                    });
                },
            ],
        ]);
        mountAt("/automations/auto-1");
        await waitFor(() => {
            expect(screen.getByTestId("test-run-toggle")).toBeInTheDocument();
        });
        // Drawer starts collapsed.
        expect(screen.queryByTestId("test-run-event-picker")).toBeNull();
        // Open it.
        fireEvent.click(screen.getByTestId("test-run-toggle"));
        await waitFor(() => {
            expect(screen.getByTestId("test-run-event-picker")).toBeInTheDocument();
        });
        // The recent-events fetch fired.
        await waitFor(() => expect(callLog).toContain("recent"));
        // Run.
        fireEvent.click(screen.getByTestId("test-run-go"));
        await waitFor(() => {
            expect(screen.getByTestId("test-run-result")).toBeInTheDocument();
        });
        expect(screen.getByTestId("test-run-outcome-success")).toBeInTheDocument();
    });
});
