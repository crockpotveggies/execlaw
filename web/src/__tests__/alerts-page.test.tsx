// Tests for the Settings → Alerts page (Phase 9.1, MIGRATION_PLAN §10).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AlertsPage } from "../settings/AlertsPage";
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

function alertRow(
    overrides: Partial<{
        id: string;
        severity: string;
        status: string;
        title: string;
        source: string;
        last_seen_at: number;
        occurrence_count: number;
        detail: string | null;
    }> = {},
) {
    return {
        id: overrides.id ?? "alert-1",
        fingerprint: "fp",
        severity: overrides.severity ?? "Error",
        source: overrides.source ?? "core.test",
        title: overrides.title ?? "Something broke",
        detail: overrides.detail ?? null,
        status: overrides.status ?? "Firing",
        first_seen_at: 1_700_000_000,
        last_seen_at: overrides.last_seen_at ?? 1_700_000_100,
        occurrence_count: overrides.occurrence_count ?? 1,
        resolved_at: null,
        resolved_by: null,
        ack_at: null,
        ack_by: null,
        snooze_until: null,
        incident_id: null,
    };
}

function listResponse(alerts: unknown[], firingCount: number) {
    return new Response(
        JSON.stringify({ alerts, firing_count: firingCount }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <AlertsPage />
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

describe("AlertsPage", () => {
    it("renders the empty hint when nothing is firing", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/alerts"))
                return listResponse([], 0);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("alerts-empty")).toBeInTheDocument();
        });
        expect(screen.getByText(/Nothing firing right now/i)).toBeInTheDocument();
    });

    it("sorts Critical above Error and shows occurrence count", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/alerts"))
                return listResponse(
                    [
                        alertRow({
                            id: "err",
                            severity: "Error",
                            title: "err title",
                            occurrence_count: 1,
                            last_seen_at: 1_700_000_200,
                        }),
                        alertRow({
                            id: "crit",
                            severity: "Critical",
                            title: "crit title",
                            occurrence_count: 5,
                            last_seen_at: 1_700_000_100,
                        }),
                    ],
                    2,
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("alert-row")).toHaveLength(2);
        });
        const rows = screen.getAllByTestId("alert-row");
        // Critical first regardless of last_seen ordering.
        expect(rows[0]).toHaveTextContent("crit title");
        expect(rows[0]).toHaveTextContent("Critical");
        expect(rows[0]).toHaveTextContent("×5");
        expect(rows[1]).toHaveTextContent("err title");
    });

    it("ack and resolve buttons POST and refresh", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        // State-driven: the "alert is gone" signal is the ack POST,
        // not the call count. AuthProvider's effects can re-fire the
        // initial GET more than once, so a "first call only" trick
        // would race with the auth resolve.
        let acked = false;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url.startsWith("/api/admin/alerts/") &&
                url.endsWith("/ack") &&
                init?.method === "POST"
            ) {
                acked = true;
                return new Response("", { status: 200 });
            }
            if (
                url.startsWith("/api/admin/alerts/") &&
                url.endsWith("/resolve") &&
                init?.method === "POST"
            ) {
                acked = true;
                return new Response("", { status: 200 });
            }
            if (url.startsWith("/api/admin/alerts")) {
                if (acked) {
                    return listResponse([], 0);
                }
                return listResponse(
                    [alertRow({ id: "a1", title: "first" })],
                    1,
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("alert-row")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("alert-ack"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/alerts/a1/ack" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
        // After ack, the list refresh produces the empty state.
        await waitFor(() => {
            expect(screen.getByTestId("alerts-empty")).toBeInTheDocument();
        });
    });

    it("toggle 'include closed' refetches without status filter", async () => {
        const calls: string[] = [];
        // State-driven: branch on whether the URL has a status filter,
        // since the toggle is what flips that param. Robust to extra
        // initial GETs from AuthProvider effect re-runs.
        fetchMock.mockImplementation(async (url: string) => {
            calls.push(url);
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/alerts")) {
                if (url.includes("status=")) {
                    return listResponse([], 0);
                }
                return listResponse(
                    [alertRow({ id: "r1", status: "Resolved", title: "old" })],
                    0,
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("alerts-empty")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("alerts-include-closed"));
        await waitFor(() => {
            expect(screen.queryByTestId("alert-row")).toBeInTheDocument();
        });
        // The unfiltered call should NOT carry the status= param.
        const lastList = [...calls].reverse().find((c) =>
            c.startsWith("/api/admin/alerts"),
        );
        expect(lastList).toBe("/api/admin/alerts?limit=200");
    });
});
