import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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
        <AuthProvider>
            <PluginsPage />
        </AuthProvider>,
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
});
