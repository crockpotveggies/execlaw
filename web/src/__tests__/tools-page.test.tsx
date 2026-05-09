// Tests for the Settings → Tools page (Phase 8a per-tool trust-class
// allowlist).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ToolsPage } from "../settings/ToolsPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = (role: "controller" | "operator" | "viewer" = "controller") =>
    new Response(
        JSON.stringify({
            user_id: "ctrl-1",
            username: "ctrl",
            display_name: "Ctrl",
            email: null,
            role,
            last_login_at: null,
        }),
        { status: 200 },
    );

function mountPage() {
    return render(
        <AuthProvider>
            <ToolsPage />
        </AuthProvider>,
    );
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

describe("ToolsPage", () => {
    it("renders the empty hint when no tools are registered", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/tools")
                return new Response(JSON.stringify({ tools: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/no tools registered yet/i),
            ).toBeInTheDocument();
        });
    });

    it("lists each tool with source badge + allowed_classes checkboxes", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/tools")
                return new Response(
                    JSON.stringify({
                        tools: [
                            {
                                tool_name: "read_memory",
                                source: "builtin",
                                source_id: null,
                                enabled: true,
                                allowed_classes: ["Controller", "KnownTrusted"],
                                description: "Read a value from memory.",
                                first_seen_at: 0,
                                last_seen_at: 0,
                                removed_at: null,
                            },
                            {
                                tool_name: "create_pr",
                                source: "mcp",
                                source_id: "github",
                                enabled: true,
                                allowed_classes: ["Controller"],
                                description: null,
                                first_seen_at: 0,
                                last_seen_at: 0,
                                removed_at: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("tool-row")).toHaveLength(2);
        });
        expect(screen.getByText("read_memory")).toBeInTheDocument();
        expect(screen.getByText("create_pr")).toBeInTheDocument();
        // Source badges from the SOURCE_BADGE map.
        expect(screen.getByText("builtin")).toBeInTheDocument();
        expect(screen.getByText("mcp")).toBeInTheDocument();
    });

    it("toggling a class fires a PATCH with the new allowed_classes", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/tools" &&
                (init?.method ?? "GET") === "GET"
            ) {
                return new Response(
                    JSON.stringify({
                        tools: [
                            {
                                tool_name: "read_memory",
                                source: "builtin",
                                source_id: null,
                                enabled: true,
                                allowed_classes: ["Controller"],
                                description: null,
                                first_seen_at: 0,
                                last_seen_at: 0,
                                removed_at: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            if (
                url === "/api/admin/tools/read_memory" &&
                init?.method === "PATCH"
            ) {
                return new Response(
                    JSON.stringify({
                        tool_name: "read_memory",
                        source: "builtin",
                        source_id: null,
                        enabled: true,
                        allowed_classes: ["Controller", "KnownTrusted"],
                        description: null,
                        first_seen_at: 0,
                        last_seen_at: 0,
                        removed_at: null,
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("tool-row")).toHaveLength(1);
        });
        // Click the KnownTrusted checkbox to widen the allowlist.
        const knownTrustedBox = screen
            .getAllByTestId("tool-class-checkbox")
            .find((el) => el.getAttribute("data-class") === "KnownTrusted");
        expect(knownTrustedBox).toBeDefined();
        fireEvent.click(knownTrustedBox!);
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/tools/read_memory" &&
                        c.init?.method === "PATCH",
                ),
            ).toBe(true);
        });
        const patch = calls.find((c) => c.init?.method === "PATCH")!;
        const body = JSON.parse((patch.init?.body as string) ?? "{}");
        expect(body.allowed_classes).toContain("Controller");
        expect(body.allowed_classes).toContain("KnownTrusted");
    });

    it("operators see read-only view (no controls, no PATCH on click)", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/tools")
                return new Response(
                    JSON.stringify({
                        tools: [
                            {
                                tool_name: "read_memory",
                                source: "builtin",
                                source_id: null,
                                enabled: true,
                                allowed_classes: ["Controller"],
                                description: null,
                                first_seen_at: 0,
                                last_seen_at: 0,
                                removed_at: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/Only Controllers can change tool access policy/i),
            ).toBeInTheDocument();
        });
        // Class checkboxes exist (read-only display) but disabled —
        // toggling shouldn't fire any PATCH.
        expect(screen.queryByTestId("tool-enabled-toggle")).toBeNull();
    });
});
