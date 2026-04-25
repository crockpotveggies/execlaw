// Tests for the Settings → MCP servers page (Phase 8c/d).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { McpServersPage } from "../settings/McpServersPage";
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
            <McpServersPage />
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
    vi.restoreAllMocks();
});

describe("McpServersPage", () => {
    it("renders the empty hint when no servers are configured", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/mcp/servers")
                return new Response(JSON.stringify({ servers: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/No MCP servers configured/i),
            ).toBeInTheDocument();
        });
    });

    it("lists each server with status badge + transport label", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/mcp/servers")
                return new Response(
                    JSON.stringify({
                        servers: [
                            {
                                id: "github",
                                display_name: "GitHub",
                                transport: "stdio",
                                command: "/usr/bin/github-mcp",
                                args: ["--repo", "owner/r"],
                                env: {},
                                cwd: null,
                                url: null,
                                auth_secret_ref: null,
                                enabled: true,
                                default_allowed_classes: ["Controller"],
                                status: "connected",
                                last_error: null,
                                created_at: 0,
                                updated_at: 0,
                            },
                            {
                                id: "broken",
                                display_name: "Broken",
                                transport: "stdio",
                                command: "/missing/cmd",
                                args: [],
                                env: {},
                                cwd: null,
                                url: null,
                                auth_secret_ref: null,
                                enabled: true,
                                default_allowed_classes: ["Controller"],
                                status: "error",
                                last_error: "spawn failed",
                                created_at: 0,
                                updated_at: 0,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("mcp-row")).toHaveLength(2);
        });
        expect(screen.getByText("GitHub")).toBeInTheDocument();
        expect(screen.getByText("connected")).toBeInTheDocument();
        expect(screen.getByText("error")).toBeInTheDocument();
        // The error badge surfaces last_error.
        expect(screen.getByText("spawn failed")).toBeInTheDocument();
    });

    it("posts the right body when adding a new server", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/mcp/servers" &&
                (init?.method ?? "GET") === "POST"
            ) {
                return new Response(JSON.stringify({}), { status: 201 });
            }
            return new Response(JSON.stringify({ servers: [] }), {
                status: 200,
            });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("mcp-add")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("mcp-add"));
        fireEvent.change(screen.getByTestId("mcp-form-id"), {
            target: { value: "github" },
        });
        fireEvent.change(screen.getByTestId("mcp-form-name"), {
            target: { value: "GitHub MCP" },
        });
        fireEvent.change(screen.getByTestId("mcp-form-command"), {
            target: { value: "/usr/local/bin/github-mcp" },
        });
        fireEvent.change(screen.getByTestId("mcp-form-args"), {
            target: { value: "--repo\nowner/repo" },
        });
        fireEvent.change(screen.getByTestId("mcp-form-env"), {
            target: { value: "GITHUB_TOKEN=abc" },
        });
        fireEvent.click(screen.getByTestId("mcp-form-save"));

        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/mcp/servers" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/mcp/servers" &&
                c.init?.method === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.id).toBe("github");
        expect(body.transport).toBe("stdio");
        expect(body.command).toBe("/usr/local/bin/github-mcp");
        expect(body.args).toEqual(["--repo", "owner/repo"]);
        expect(body.env).toEqual({ GITHUB_TOKEN: "abc" });
    });

    it("operators see read-only view (no add / edit / delete)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/mcp/servers")
                return new Response(
                    JSON.stringify({
                        servers: [
                            {
                                id: "github",
                                display_name: "GitHub",
                                transport: "stdio",
                                command: "/x",
                                args: [],
                                env: {},
                                cwd: null,
                                url: null,
                                auth_secret_ref: null,
                                enabled: true,
                                default_allowed_classes: ["Controller"],
                                status: "connected",
                                last_error: null,
                                created_at: 0,
                                updated_at: 0,
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
                screen.getByText(/Only Controllers can manage MCP servers/i),
            ).toBeInTheDocument();
        });
        expect(screen.queryByTestId("mcp-add")).toBeNull();
        expect(screen.queryByTestId("mcp-edit")).toBeNull();
        expect(screen.queryByTestId("mcp-delete")).toBeNull();
    });
});
