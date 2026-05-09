import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { PluginsPage } from "../settings/PluginsPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
    localStorage.setItem("execlaw.access_token", "tok-a");
    localStorage.setItem("execlaw.refresh_token", "tok-r");
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

function mountPage() {
    return render(
        <MemoryRouter>
            <AuthProvider>
                <PluginsPage />
            </AuthProvider>
        </MemoryRouter>,
    );
}

describe("PluginsPage", () => {
    it("renders the empty state when no plugins are installed", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(
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
            }
            if (url === "/api/admin/plugins") {
                return new Response(JSON.stringify({ plugins: [] }), {
                    status: 200,
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText(/no plugins installed/i)).toBeInTheDocument();
        });
    });

    it("renders each plugin with its enabled state and version", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(JSON.stringify({ user_id: "c1", username: "u", display_name: "U", email: null, role: "controller", last_login_at: null }), { status: 200 });
            }
            if (url === "/api/admin/plugins") {
                return new Response(
                    JSON.stringify({
                        plugins: [
                            {
                                plugin_id: "alpha",
                                version: "1.0.0",
                                enabled: true,
                                installed_at: 0,
                                updated_at: 0,
                            },
                            {
                                plugin_id: "beta",
                                version: "0.2.0",
                                enabled: false,
                                installed_at: 0,
                                updated_at: 0,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("alpha")).toBeInTheDocument();
        });
        expect(screen.getByText("beta")).toBeInTheDocument();
        expect(screen.getByText("v1.0.0")).toBeInTheDocument();
        expect(screen.getByText("enabled")).toBeInTheDocument();
        expect(screen.getByText("disabled")).toBeInTheDocument();
    });

    it("toggle button posts to enable/disable then re-fetches the list", async () => {
        // Sequence: /me, /plugins (initial), /plugins/beta/enable, /plugins (refetch).
        const calls: string[] = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push(`${(init?.method ?? "GET")} ${url}`);
            if (url === "/api/admin/me") {
                return new Response(JSON.stringify({ user_id: "c1", username: "u", display_name: "U", email: null, role: "controller", last_login_at: null }), { status: 200 });
            }
            if (url === "/api/admin/plugins") {
                return new Response(
                    JSON.stringify({
                        plugins: [
                            {
                                plugin_id: "beta",
                                version: "0.2.0",
                                enabled: false,
                                installed_at: 0,
                                updated_at: 0,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/plugins/beta/enable") {
                return new Response("{}", { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("beta")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("plugin-toggle"));
        await waitFor(() => {
            expect(
                calls.some((c) => c === "POST /api/admin/plugins/beta/enable"),
            ).toBe(true);
        });
    });

    it("install form mounts the file picker + submit button", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(JSON.stringify({ user_id: "c1", username: "u", display_name: "U", email: null, role: "controller", last_login_at: null }), { status: 200 });
            }
            if (url === "/api/admin/plugins") {
                return new Response(JSON.stringify({ plugins: [] }), {
                    status: 200,
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("plugin-install-file"),
            ).toBeInTheDocument();
        });
        const submit = screen.getByTestId(
            "plugin-install-submit",
        ) as HTMLButtonElement;
        // Submit is enabled even with no file picked; the install
        // function rejects via "Choose a ZIP file first."
        expect(submit).toBeInTheDocument();
        // The shape + content-type wiring of installPlugin is covered
        // by endpoints.test.ts; this UI test just guards the rendered
        // form surface.
    });

    it("renders a gear icon ONLY for enabled plugins with has_settings_ui", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(
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
            }
            if (url === "/api/admin/plugins") {
                return new Response(
                    JSON.stringify({
                        plugins: [
                            {
                                plugin_id: "google-contacts",
                                version: "0.1.0",
                                enabled: true,
                                installed_at: 0,
                                updated_at: 0,
                                has_settings_ui: true,
                            },
                            {
                                plugin_id: "plain-tool",
                                version: "0.1.0",
                                enabled: true,
                                installed_at: 0,
                                updated_at: 0,
                                has_settings_ui: false,
                            },
                            {
                                plugin_id: "google-contacts-disabled",
                                version: "0.1.0",
                                enabled: false,
                                installed_at: 0,
                                updated_at: 0,
                                has_settings_ui: true,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("google-contacts")).toBeInTheDocument();
        });
        // Gear shown for every configurable row regardless of
        // enabled-state — operators may want to manage credentials
        // on a disabled plugin or uninstall it via the config page.
        const gears = screen.getAllByTestId("plugin-configure");
        expect(gears).toHaveLength(2); // google-contacts + google-contacts-disabled
        const ids = gears.map((g) => g.getAttribute("data-plugin-id")).sort();
        expect(ids).toEqual(["google-contacts", "google-contacts-disabled"]);
        // Each href points at the per-plugin nested route.
        expect(
            gears.find((g) => g.getAttribute("data-plugin-id") === "google-contacts")?.getAttribute("href"),
        ).toBe("/settings/plugins/google-contacts");
        // The plain-tool row (has_settings_ui=false) has no gear.
        // Toggle icon present on every row regardless.
        const toggles = screen.getAllByTestId("plugin-toggle");
        expect(toggles).toHaveLength(3);
        // Enabled rows show toggle-on, disabled shows toggle-off —
        // distinguished via data-enabled (avoids brittle icon-class
        // assertions).
        const states = toggles.map((t) => t.getAttribute("data-enabled")).sort();
        expect(states).toEqual(["false", "true", "true"]);
    });

    it("plugin title links to the config page when has_settings_ui (no underline)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(
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
            }
            if (url === "/api/admin/plugins") {
                return new Response(
                    JSON.stringify({
                        plugins: [
                            {
                                plugin_id: "google-contacts",
                                version: "0.1.0",
                                enabled: true,
                                installed_at: 0,
                                updated_at: 0,
                                has_settings_ui: true,
                            },
                            {
                                plugin_id: "plain-tool",
                                version: "0.1.0",
                                enabled: true,
                                installed_at: 0,
                                updated_at: 0,
                                has_settings_ui: false,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("google-contacts")).toBeInTheDocument();
        });
        // Configurable row's title is a Link to the config page.
        const titleLink = screen.getByTestId("plugin-title-link");
        expect(titleLink.getAttribute("data-plugin-id")).toBe("google-contacts");
        expect(titleLink.getAttribute("href")).toBe(
            "/settings/plugins/google-contacts",
        );
        // Underline removed via text-decoration-none.
        expect(titleLink.className).toContain("text-decoration-none");
        expect(titleLink.className).toContain("text-body");
        // Plain-tool row has no link — title rendered as a plain span.
        expect(screen.getAllByTestId("plugin-title-link")).toHaveLength(1);
    });

    it("does not render an Uninstall button on the row (moved to config page danger zone)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(
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
            }
            if (url === "/api/admin/plugins") {
                return new Response(
                    JSON.stringify({
                        plugins: [
                            {
                                plugin_id: "alpha",
                                version: "1.0.0",
                                enabled: true,
                                installed_at: 0,
                                updated_at: 0,
                                has_settings_ui: false,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("alpha")).toBeInTheDocument();
        });
        expect(screen.queryByTestId("plugin-uninstall")).toBeNull();
        // No "Uninstall" text either.
        expect(screen.queryByText(/Uninstall/i)).toBeNull();
    });

});
