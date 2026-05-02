// Tests for the PluginConfigShell (PluginConfigRouter) — the
// per-plugin config page wrapper that owns the chrome (header
// back-button + danger-zone footer with Uninstall).
//
// Pinned invariants:
//   * Danger zone with Uninstall button is rendered for EVERY
//     plugin, regardless of whether the plugin has a registered
//     config component or falls through to the "no config UI"
//     stub. Plugins cannot omit it.
//   * Click → confirm → DELETE /api/admin/plugins/<id> → navigate
//     back to /settings/plugins.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { PluginConfigRouter } from "../settings/PluginConfigRouter";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = () =>
    new Response(
        JSON.stringify({
            user_id: "c1",
            username: "u",
            display_name: "U",
            email: null,
            role: "controller",
            last_login_at: null,
        }),
        { status: 200 },
    );

const pluginsResponse = (plugin_id: string, has_settings_ui: boolean) =>
    new Response(
        JSON.stringify({
            plugins: [
                {
                    plugin_id,
                    version: "0.1.0",
                    enabled: true,
                    installed_at: 0,
                    updated_at: 0,
                    has_settings_ui,
                },
            ],
        }),
        { status: 200 },
    );

function mountAt(path: string) {
    return render(
        <AuthProvider>
            <MemoryRouter initialEntries={[path]}>
                <Routes>
                    <Route
                        path="/settings/plugins/:plugin_id"
                        element={<PluginConfigRouter />}
                    />
                    <Route
                        path="/settings/plugins"
                        element={<div data-testid="plugins-list-page">PLUGINS</div>}
                    />
                </Routes>
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
    vi.restoreAllMocks();
});

describe("PluginConfigShell", () => {
    it("renders the danger zone with Uninstall for plugins WITH a registered config", async () => {
        // google-contacts has a registered config component, so
        // the body renders the OAuth form. The shell's danger
        // zone must still appear underneath it.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins")
                return pluginsResponse("google-contacts", true);
            if (
                url ===
                "/api/admin/oauth/clients/google-contacts/controller"
            )
                return new Response(
                    JSON.stringify({
                        error: "oauth_not_found",
                        message: "no oauth client",
                    }),
                    { status: 404 },
                );
            return new Response("{}", { status: 200 });
        });
        mountAt("/settings/plugins/google-contacts");
        await waitFor(() => {
            expect(
                screen.getByTestId("plugin-config-danger-zone"),
            ).toBeInTheDocument();
        });
        const btn = screen.getByTestId("plugin-config-uninstall");
        expect(btn).toBeInTheDocument();
        expect(btn.textContent).toMatch(/Uninstall google-contacts/i);
    });

    it("renders the danger zone with Uninstall even for plugins WITHOUT a registered config", async () => {
        // Plugin without a registered config component falls
        // through to the "no configuration UI available" stub.
        // The danger zone must STILL be there — operators need a
        // path to uninstall any installed plugin.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins")
                return pluginsResponse("unregistered-plugin", false);
            return new Response("{}", { status: 200 });
        });
        mountAt("/settings/plugins/unregistered-plugin");
        await waitFor(() => {
            expect(
                screen.getByTestId("plugin-config-danger-zone"),
            ).toBeInTheDocument();
        });
        // The fallback stub is also present.
        expect(
            screen.getByText(/No configuration UI available/i),
        ).toBeInTheDocument();
        // Uninstall button still present.
        expect(screen.getByTestId("plugin-config-uninstall")).toBeInTheDocument();
    });

    it("clicking Uninstall (after confirm) DELETEs and navigates back to /settings/plugins", async () => {
        const calls: string[] = [];
        fetchMock.mockImplementation(
            async (url: string, init?: RequestInit) => {
                calls.push(`${(init?.method ?? "GET")} ${url}`);
                if (url === "/api/admin/me") return meResponse();
                if (url === "/api/admin/plugins" && (init?.method ?? "GET") === "GET")
                    return pluginsResponse("test-plugin", false);
                if (url === "/api/admin/plugins/test-plugin" && init?.method === "DELETE")
                    return new Response("{}", { status: 200 });
                return new Response("{}", { status: 200 });
            },
        );
        // Confirm dialog → accept.
        vi.stubGlobal("confirm", () => true);
        mountAt("/settings/plugins/test-plugin");
        await waitFor(() => {
            expect(screen.getByTestId("plugin-config-uninstall")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("plugin-config-uninstall"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) => c === "DELETE /api/admin/plugins/test-plugin",
                ),
            ).toBe(true);
        });
        // After uninstall, navigate replaces the route with /settings/plugins.
        await waitFor(() => {
            expect(screen.getByTestId("plugins-list-page")).toBeInTheDocument();
        });
    });

    it("declining the confirm dialog does NOT issue the DELETE", async () => {
        const calls: string[] = [];
        fetchMock.mockImplementation(
            async (url: string, init?: RequestInit) => {
                calls.push(`${(init?.method ?? "GET")} ${url}`);
                if (url === "/api/admin/me") return meResponse();
                if (url === "/api/admin/plugins")
                    return pluginsResponse("test-plugin", false);
                return new Response("{}", { status: 200 });
            },
        );
        vi.stubGlobal("confirm", () => false);
        mountAt("/settings/plugins/test-plugin");
        await waitFor(() => {
            expect(screen.getByTestId("plugin-config-uninstall")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("plugin-config-uninstall"));
        // Give any in-flight async a tick.
        await new Promise((r) => setTimeout(r, 50));
        expect(
            calls.some((c) => c.startsWith("DELETE /api/admin/plugins/")),
        ).toBe(false);
        // Still on the config page.
        expect(screen.queryByTestId("plugins-list-page")).toBeNull();
    });

    it("rejects malformed routes (literal 'undefined'/'null') without firing requests", async () => {
        // Defensive guard: a stale upstream link can generate
        // /settings/plugins/undefined when an undefined value
        // gets stringified by encodeURIComponent. The shell
        // must show a clear error and refuse to mount the
        // inner Component (which would otherwise fire
        // /api/admin/oauth/clients/undefined/controller).
        const calls: string[] = [];
        fetchMock.mockImplementation(async (url: string) => {
            calls.push(url);
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins")
                return new Response(
                    JSON.stringify({ plugins: [] }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountAt("/settings/plugins/undefined");
        await waitFor(() => {
            expect(
                screen.getByText(/Invalid plugin id in URL/i),
            ).toBeInTheDocument();
        });
        // The dangerous request never fires.
        expect(
            calls.some((c) =>
                c.includes("/api/admin/oauth/clients/undefined/"),
            ),
        ).toBe(false);
        // Danger zone IS still rendered (operator can still
        // attempt uninstall on a misrouted page).
        expect(screen.getByTestId("plugin-config-danger-zone")).toBeInTheDocument();
    });

    it("renders the Back to Plugins link", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins")
                return pluginsResponse("any-plugin", false);
            return new Response("{}", { status: 200 });
        });
        mountAt("/settings/plugins/any-plugin");
        await waitFor(() => {
            expect(screen.getByTestId("plugin-config-back")).toBeInTheDocument();
        });
        const back = screen.getByTestId("plugin-config-back");
        expect(back.getAttribute("href")).toBe("/settings/plugins");
    });
});
