// Tests for the Settings → Skills page.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { SkillsPage } from "../settings/SkillsPage";
import { AuthProvider } from "../auth/AuthContext";

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

function tool(
    overrides: Partial<{
        tool_name: string;
        source: "builtin" | "plugin" | "mcp";
        source_id: string | null;
        enabled: boolean;
        description: string | null;
        removed_at: number | null;
    }>,
) {
    return {
        tool_name: overrides.tool_name ?? "tool-x",
        source: overrides.source ?? "builtin",
        source_id: overrides.source_id ?? null,
        enabled: overrides.enabled ?? true,
        allowed_classes: ["Controller"],
        description: overrides.description ?? null,
        first_seen_at: 1_700_000_000,
        last_seen_at: 1_700_000_000,
        removed_at: overrides.removed_at ?? null,
    };
}

function listResponse(tools: unknown[]) {
    return new Response(JSON.stringify({ tools }), { status: 200 });
}

function mountPage() {
    return render(
        <AuthProvider>
            <MemoryRouter>
                <SkillsPage />
            </MemoryRouter>
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

describe("SkillsPage", () => {
    it("renders the empty hint when no tools are registered", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/tools") return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("skills-empty")).toBeInTheDocument();
        });
    });

    it("groups tools by source and orders Built-in → Plugin → MCP", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/tools")
                return listResponse([
                    tool({
                        tool_name: "send_message",
                        source: "plugin",
                        source_id: "google-calendar",
                    }),
                    tool({
                        tool_name: "set_thread_name",
                        source: "builtin",
                    }),
                    tool({
                        tool_name: "search_db",
                        source: "mcp",
                        source_id: "postgres",
                    }),
                    tool({
                        tool_name: "create_event",
                        source: "plugin",
                        source_id: "google-calendar",
                    }),
                ]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("skill-group")).toHaveLength(3);
        });
        const groups = screen.getAllByTestId("skill-group");
        // Order: Built-in → Plugin (google-calendar) → MCP (postgres).
        expect(groups[0]).toHaveTextContent("Built-in");
        expect(groups[1]).toHaveTextContent("Plugin · google-calendar");
        expect(groups[2]).toHaveTextContent("MCP · postgres");
        // Plugin group has 2 tools; the count is rendered.
        expect(groups[1]).toHaveTextContent("2 tools");
    });

    it("excludes tombstoned tools and shows disabled marker", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/tools")
                return listResponse([
                    tool({
                        tool_name: "alive",
                        source: "builtin",
                        enabled: true,
                    }),
                    tool({
                        tool_name: "disabled",
                        source: "builtin",
                        enabled: false,
                    }),
                    tool({
                        tool_name: "tombstoned",
                        source: "builtin",
                        removed_at: 1_700_000_000,
                    }),
                ]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("alive")).toBeInTheDocument();
        });
        // Disabled tool shows the marker text.
        expect(screen.getByText(/\(disabled\)/i)).toBeInTheDocument();
        // Tombstoned doesn't render at all.
        expect(screen.queryByText("tombstoned")).toBeNull();
    });
});
