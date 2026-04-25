// Tests for the Settings → Runners view-only page (Phase 8.5).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { RunnersPage } from "../settings/RunnersPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = (role: "controller" | "operator" = "controller") =>
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
            <RunnersPage />
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

describe("RunnersPage", () => {
    it("renders the empty hint when there are no active runners", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners")
                return new Response(
                    JSON.stringify({ runners: [], idle_ttl_secs: 600 }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/No active runners/i),
            ).toBeInTheDocument();
        });
    });

    it("renders one row per runner with controller badge + idle countdown", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                conversation_id: "ctrl-conv",
                                principal_label: "controller",
                                modality: "Text",
                                controller_runner: true,
                                started_at: 0,
                                last_active_at: 0,
                                in_flight: false,
                                turn_count: 12,
                                restart_pending: false,
                                idle_secs_remaining: null,
                            },
                            {
                                conversation_id: "earl-conv",
                                principal_label: "signal:+1555earl",
                                modality: "Text",
                                controller_runner: false,
                                started_at: 0,
                                last_active_at: 0,
                                in_flight: false,
                                turn_count: 3,
                                restart_pending: false,
                                idle_secs_remaining: 480,
                            },
                        ],
                        idle_ttl_secs: 600,
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("runner-row")).toHaveLength(2);
        });
        // Controller badge appears for ctrl-conv only.
        expect(
            screen.getByText(/controller · always hot/i),
        ).toBeInTheDocument();
        // Idle countdown for earl-conv.
        expect(screen.getByText(/idle in/i)).toBeInTheDocument();
        expect(screen.getByText(/8m00s/)).toBeInTheDocument();
    });

    it("posts /restart and refreshes when the button is clicked", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                conversation_id: "stuck-conv",
                                principal_label: "Alice",
                                modality: "Text",
                                controller_runner: false,
                                started_at: 0,
                                last_active_at: 0,
                                in_flight: true,
                                turn_count: 1,
                                restart_pending: false,
                                idle_secs_remaining: null,
                            },
                        ],
                        idle_ttl_secs: 600,
                    }),
                    { status: 200 },
                );
            if (
                url === "/api/admin/runners/stuck-conv/restart" &&
                init?.method === "POST"
            ) {
                return new Response("{}", { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
        // Auto-confirm the restart dialog.
        vi.spyOn(window, "confirm").mockImplementation(() => true);
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("runner-row")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("runner-restart"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/runners/stuck-conv/restart" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
    });

    it("operators see no Restart button (read-only)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/runners")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                conversation_id: "c",
                                principal_label: null,
                                modality: "Text",
                                controller_runner: false,
                                started_at: 0,
                                last_active_at: 0,
                                in_flight: false,
                                turn_count: 0,
                                restart_pending: false,
                                idle_secs_remaining: 600,
                            },
                        ],
                        idle_ttl_secs: 600,
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/Only Controllers can restart runners/i),
            ).toBeInTheDocument();
        });
        expect(screen.queryByTestId("runner-restart")).toBeNull();
    });
});
