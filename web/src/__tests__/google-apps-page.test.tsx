// Tests for the Settings → Google Apps page (consolidated Google
// plugin).
//
// Three behaviours pinned:
//   1. The OAuth section renders the union-scope client form.
//   2. The Modules section shows five toggles, each pre-populated from
//      `GET /api/admin/plugins/google-apps/settings/enabled_modules`.
//   3. Clicking a toggle fires `PUT` with the right JSON-array body.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { GoogleAppsPage } from "../settings/GoogleAppsPage";
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

function mountPage() {
    return render(
        <AuthProvider>
            <GoogleAppsPage pluginId="google-apps" pluginVersion="0.1.0" />
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

describe("GoogleAppsPage", () => {
    it("renders five module toggles all checked by default when the setting is unset", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/oauth/clients/google-apps/controller") {
                return new Response(
                    JSON.stringify({
                        error: "oauth_not_found",
                        message: "no oauth client",
                    }),
                    { status: 404 },
                );
            }
            if (
                url ===
                "/api/admin/plugins/google-apps/settings/enabled_modules"
            ) {
                return new Response(
                    JSON.stringify({
                        error: "plugin_setting_not_found",
                        message: "no value",
                    }),
                    { status: 404 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("google-apps-module-toggles"),
            ).toBeInTheDocument();
        });
        for (const id of ["gmail", "calendar", "contacts", "tasks", "drive"]) {
            const toggle = screen.getByTestId(
                `google-apps-toggle-${id}`,
            ) as HTMLInputElement;
            expect(toggle.checked).toBe(true);
        }
    });

    it("reflects a partial enabled_modules setting from the backend", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/oauth/clients/google-apps/controller") {
                return new Response(
                    JSON.stringify({
                        error: "oauth_not_found",
                        message: "no oauth client",
                    }),
                    { status: 404 },
                );
            }
            if (
                url ===
                "/api/admin/plugins/google-apps/settings/enabled_modules"
            ) {
                return new Response(
                    JSON.stringify({
                        plugin_id: "google-apps",
                        key: "enabled_modules",
                        value: JSON.stringify(["gmail", "calendar"]),
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("google-apps-toggle-gmail"),
            ).toBeInTheDocument();
        });
        const gmail = screen.getByTestId(
            "google-apps-toggle-gmail",
        ) as HTMLInputElement;
        const calendar = screen.getByTestId(
            "google-apps-toggle-calendar",
        ) as HTMLInputElement;
        const drive = screen.getByTestId(
            "google-apps-toggle-drive",
        ) as HTMLInputElement;
        expect(gmail.checked).toBe(true);
        expect(calendar.checked).toBe(true);
        expect(drive.checked).toBe(false);
    });

    it("PUTs the new enabled_modules array when a toggle is clicked", async () => {
        let putBody: Record<string, unknown> | null = null;
        fetchMock.mockImplementation(
            async (url: string, init?: RequestInit) => {
                if (url === "/api/admin/me") return meResponse();
                if (
                    url ===
                    "/api/admin/oauth/clients/google-apps/controller"
                ) {
                    return new Response(
                        JSON.stringify({
                            error: "oauth_not_found",
                            message: "no oauth client",
                        }),
                        { status: 404 },
                    );
                }
                if (
                    url ===
                        "/api/admin/plugins/google-apps/settings/enabled_modules" &&
                    (init?.method ?? "GET") === "GET"
                ) {
                    return new Response(
                        JSON.stringify({
                            plugin_id: "google-apps",
                            key: "enabled_modules",
                            value: JSON.stringify([
                                "gmail",
                                "calendar",
                                "contacts",
                                "tasks",
                                "drive",
                            ]),
                        }),
                        { status: 200 },
                    );
                }
                if (
                    url ===
                        "/api/admin/plugins/google-apps/settings/enabled_modules" &&
                    init?.method === "PUT"
                ) {
                    putBody = JSON.parse(init.body as string);
                    return new Response(
                        JSON.stringify({
                            plugin_id: "google-apps",
                            key: "enabled_modules",
                            value: (putBody as { value: string }).value,
                        }),
                        { status: 200 },
                    );
                }
                return new Response("{}", { status: 200 });
            },
        );
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("google-apps-toggle-drive"),
            ).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("google-apps-toggle-drive"));
        await waitFor(() => {
            expect(putBody).not.toBeNull();
        });
        // Body shape: { value: "[\"gmail\",\"calendar\",\"contacts\",\"tasks\"]" }
        const valueStr = (putBody as unknown as { value: string }).value;
        const arr = JSON.parse(valueStr) as string[];
        expect(arr).toContain("gmail");
        expect(arr).toContain("calendar");
        expect(arr).not.toContain("drive");
        expect(arr).toHaveLength(4);
    });
});
