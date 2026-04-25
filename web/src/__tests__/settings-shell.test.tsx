// Smoke tests for the settings shell — verifies the tab bar mounts,
// /settings → /settings/plugins redirect works, and each page lazily
// fetches its endpoint. Per-page logic is covered separately.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { Settings } from "../settings/Settings";
import { AuthProvider } from "../auth/AuthContext";
import { __resetChatStore } from "../chat/store";

let fetchMock: ReturnType<typeof vi.fn>;

function mountAt(path: string) {
    // Mirror App's `<Route path="/settings/*" element={<Settings />}/>`
    // so the inner Routes inside Settings resolves relative to /settings.
    return render(
        <AuthProvider>
            <MemoryRouter initialEntries={[path]}>
                <Routes>
                    <Route path="/settings/*" element={<Settings />} />
                </Routes>
            </MemoryRouter>
        </AuthProvider>,
    );
}

beforeEach(() => {
    // Pretend we're already authenticated so AuthContext doesn't redirect.
    localStorage.setItem("execlaw.access_token", "tok-a");
    localStorage.setItem("execlaw.refresh_token", "tok-r");

    fetchMock = vi.fn().mockImplementation(async (url: string) => {
        if (url === "/api/admin/me") {
            return new Response(
                JSON.stringify({
                    user_id: "controller-1",
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
            return new Response(JSON.stringify({ plugins: [] }), { status: 200 });
        }
        if (url === "/api/admin/principals") {
            return new Response(
                JSON.stringify({ principals: [] }),
                { status: 200 },
            );
        }
        if (url === "/api/admin/hardware") {
            return new Response(JSON.stringify({ gpus: [] }), { status: 200 });
        }
        if (url === "/api/admin/backends") {
            return new Response(
                JSON.stringify({
                    backends: [
                        "Standard",
                        "Small",
                        "VoiceSTT",
                        "VoiceTTS",
                    ].map((purpose) => ({ purpose, configured: false, backend: null })),
                }),
                { status: 200 },
            );
        }
        if (url === "/api/admin/users") {
            return new Response(JSON.stringify({ users: [] }), { status: 200 });
        }
        if (url === "/api/admin/webauthn/credentials") {
            return new Response(
                JSON.stringify({ credentials: [] }),
                { status: 200 },
            );
        }
        if (url.startsWith("/api/admin/logs")) {
            return new Response(JSON.stringify({ entries: [] }), { status: 200 });
        }
        if (url.startsWith("/api/admin/eval/flags")) {
            return new Response(JSON.stringify({ flags: [] }), { status: 200 });
        }
        return new Response("{}", { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
    __resetChatStore();
});

describe("Settings shell", () => {
    it("/settings redirects to user (the new index)", async () => {
        // User page makes calls to /api/admin/users +
        // /api/admin/webauthn/credentials; the default mock returns
        // empty lists for both via the catch-all branch.
        mountAt("/settings");
        await waitFor(() => {
            expect(screen.getByTestId("settings-user")).toBeInTheDocument();
        });
    });

    it("renders the post-Phase-8.7 tab set", async () => {
        mountAt("/settings/plugins");
        await waitFor(() => {
            expect(screen.getByTestId("settings-plugins")).toBeInTheDocument();
        });
        for (const label of [
            "User",
            "Plugins",
            "Backends",
            "Principals",
            "Logs",
            "Eval flags",
        ]) {
            expect(
                screen.getByRole("link", { name: new RegExp(label, "i") }),
            ).toBeInTheDocument();
        }
        // Old tabs that have been merged elsewhere should be gone.
        expect(
            screen.queryByRole("link", { name: /Hardware/i }),
        ).toBeNull();
        expect(
            screen.queryByRole("link", { name: /Profile/i }),
        ).toBeNull();
        // "Login" used to be the consolidated tab name; ensure it's
        // not lingering after the Phase-8.7 rename. We can't simply
        // queryByRole({ name: /Login/ }) — that would match the
        // "Login" subdir of /settings/login if the legacy redirect
        // is configured as a Route. The redirect is via Navigate so
        // there's no link with that label in the rendered tab bar.
        const tabLinks = screen.getAllByRole("link");
        const labels = tabLinks.map((a) => a.textContent?.trim() ?? "");
        expect(labels).not.toContain("Login");
        expect(labels).not.toContain("Users");
    });

    it("clicking Backends loads the Backends pane (with the inline Hardware section)", async () => {
        mountAt("/settings/plugins");
        await waitFor(() => {
            expect(screen.getByTestId("settings-plugins")).toBeInTheDocument();
        });
        fireEvent.click(
            screen.getByRole("link", { name: /Backends/i }),
        );
        await waitFor(() => {
            expect(screen.getByTestId("settings-backends")).toBeInTheDocument();
        });
        // Hardware section now lives inside Backends.
        await waitFor(() => {
            expect(screen.getByTestId("settings-hardware")).toBeInTheDocument();
        });
    });
});
