// Tests for the Settings → Runners view (Phase 16: supervisor-tracked).

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
            if (url === "/api/admin/runners/groups")
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

    it("shows a friendly hint when the supervisor is disabled (503)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners/groups")
                return new Response(
                    JSON.stringify({
                        error: {
                            code: "runners_disabled",
                            message: "runner supervisor not configured",
                        },
                    }),
                    { status: 503 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/Runner supervisor is disabled/i),
            ).toBeInTheDocument();
        });
    });

    it("renders one row per runner with controller badge + idle countdown", async () => {
        const now = Math.floor(Date.now() / 1000);
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners/groups")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                group_id: "g-controller-uuid-aaaa",
                                controller_runner: true,
                                status: "ready",
                                started_at: now - 30,
                                last_active_at: now - 5,
                                in_flight_turns: 0,
                                container_id: "abc123def456",
                            },
                            {
                                group_id: "g-earl-uuid-bbbb",
                                controller_runner: false,
                                status: "ready",
                                // 120s ago, idle TTL 600s → "idle in 8m00s".
                                started_at: now - 200,
                                last_active_at: now - 120,
                                in_flight_turns: 0,
                                container_id: "789xyz012abc",
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
        // Controller badge appears for the controller's group only.
        expect(
            screen.getByText(/controller · always hot/i),
        ).toBeInTheDocument();
        // Idle countdown shows for non-controller, non-busy runners.
        expect(screen.getByText(/idle in/i)).toBeInTheDocument();
        // 600 - 120 = 480 = 8m00s.
        expect(screen.getByText(/8m00s/)).toBeInTheDocument();
    });

    it("renders an in-flight badge when a turn is running", async () => {
        const now = Math.floor(Date.now() / 1000);
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners/groups")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                group_id: "busy-group",
                                controller_runner: false,
                                status: "ready",
                                started_at: now - 10,
                                last_active_at: now,
                                in_flight_turns: 2,
                                container_id: "cid",
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
            expect(screen.getByText(/2 in flight/i)).toBeInTheDocument();
        });
    });

    it("posts /restart with the group_id and refreshes", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners/groups")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                group_id: "stuck-group-uuid",
                                controller_runner: false,
                                status: "ready",
                                started_at: 0,
                                last_active_at: 0,
                                in_flight_turns: 1,
                                container_id: "cid",
                            },
                        ],
                        idle_ttl_secs: 600,
                    }),
                    { status: 200 },
                );
            if (
                url === "/api/admin/runners/groups/stuck-group-uuid/restart" &&
                init?.method === "POST"
            ) {
                return new Response("{}", { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
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
                        c.url ===
                            "/api/admin/runners/groups/stuck-group-uuid/restart" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
    });

    it("posts /wipe with the group_id when Wipe is clicked", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/runners/groups")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                group_id: "g-wipe-me",
                                controller_runner: false,
                                status: "ready",
                                started_at: 0,
                                last_active_at: 0,
                                in_flight_turns: 0,
                                container_id: "cid",
                            },
                        ],
                        idle_ttl_secs: 600,
                    }),
                    { status: 200 },
                );
            if (
                url === "/api/admin/runners/groups/g-wipe-me/wipe" &&
                init?.method === "POST"
            ) {
                return new Response("{}", { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
        vi.spyOn(window, "confirm").mockImplementation(() => true);
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("runner-wipe")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("runner-wipe"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/runners/groups/g-wipe-me/wipe" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
    });

    it("operators see no Restart / Wipe buttons (read-only)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/runners/groups")
                return new Response(
                    JSON.stringify({
                        runners: [
                            {
                                group_id: "g",
                                controller_runner: false,
                                status: "ready",
                                started_at: 0,
                                last_active_at: 0,
                                in_flight_turns: 0,
                                container_id: null,
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
                screen.getByText(/Only Controllers can mutate runners/i),
            ).toBeInTheDocument();
        });
        expect(screen.queryByTestId("runner-restart")).toBeNull();
        expect(screen.queryByTestId("runner-wipe")).toBeNull();
    });
});
