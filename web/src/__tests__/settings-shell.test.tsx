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
    it("/settings redirects to plugins by default", async () => {
        mountAt("/settings");
        await waitFor(() => {
            expect(screen.getByTestId("settings-plugins")).toBeInTheDocument();
        });
    });

    it("renders the five tab links", async () => {
        mountAt("/settings/plugins");
        // Wait for the auth-bootstrap /me probe + plugin list to complete
        // so the settings shell is mounted before we query tab labels.
        await waitFor(() => {
            expect(screen.getByTestId("settings-plugins")).toBeInTheDocument();
        });
        for (const label of [
            "Plugins",
            "Principals",
            "Hardware",
            "Logs",
            "Eval flags",
        ]) {
            expect(
                screen.getByRole("link", { name: new RegExp(label, "i") }),
            ).toBeInTheDocument();
        }
    });

    it("clicking the Hardware tab swaps the active page", async () => {
        mountAt("/settings/plugins");
        await waitFor(() => {
            expect(screen.getByTestId("settings-plugins")).toBeInTheDocument();
        });
        fireEvent.click(
            screen.getByRole("link", { name: /Hardware/i }),
        );
        await waitFor(() => {
            expect(screen.getByTestId("settings-hardware")).toBeInTheDocument();
        });
    });
});
