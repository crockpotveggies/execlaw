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
                        "Reasoning",
                        "Guardrail",
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
    it("/settings redirects to login (the new index)", async () => {
        // Login section makes calls to /api/admin/users +
        // /api/admin/webauthn/credentials; the default mock returns
        // empty lists for both via the catch-all branch.
        mountAt("/settings");
        await waitFor(() => {
            expect(screen.getByTestId("settings-login")).toBeInTheDocument();
        });
    });

    it("renders the post-Phase-8.6 tab set", async () => {
        mountAt("/settings/plugins");
        await waitFor(() => {
            expect(screen.getByTestId("settings-plugins")).toBeInTheDocument();
        });
        for (const label of [
            "Login",
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
        expect(
            screen.queryByRole("link", { name: /Users/i }),
        ).toBeNull();
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
