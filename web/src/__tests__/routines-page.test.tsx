// Tests for the Settings → Routines page (Phase 10, §5.6).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { RoutinesPage } from "../settings/RoutinesPage";
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

function routine(
    overrides: Partial<{
        id: string;
        name: string;
        schedule_cron: string;
        last_run_status: string | null;
        enabled: boolean;
        next_run_at: number | null;
    }> = {},
) {
    return {
        id: overrides.id ?? "r-1",
        name: overrides.name ?? "morning",
        schedule_cron: overrides.schedule_cron ?? "0 8 * * *",
        timezone: "UTC",
        prompt: "do",
        target_conversation_id: null,
        enabled: overrides.enabled ?? true,
        last_run_at: null,
        last_run_status: overrides.last_run_status ?? null,
        next_run_at: overrides.next_run_at ?? 1_900_000_000,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    };
}

function listResponse(routines: unknown[]) {
    return new Response(JSON.stringify({ routines }), { status: 200 });
}

function previewResponse(times: number[]) {
    return new Response(
        JSON.stringify({ next_fires_unix: times }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <RoutinesPage />
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

describe("RoutinesPage", () => {
    it("renders empty state and the new-routine button", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/routines") return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("routines-empty")).toBeInTheDocument();
        });
        expect(screen.getByTestId("routines-new")).toBeInTheDocument();
    });

    it("opens editor on new, fetches preview, and POSTs on save", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        let created = false;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/routines/preview" &&
                init?.method === "POST"
            ) {
                return previewResponse([
                    1_900_000_000,
                    1_900_086_400,
                    1_900_172_800,
                ]);
            }
            if (
                url === "/api/admin/routines" &&
                init?.method === "POST"
            ) {
                created = true;
                return new Response(JSON.stringify(routine()), { status: 200 });
            }
            if (url === "/api/admin/routines") {
                return created ? listResponse([routine()]) : listResponse([]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("routines-new")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("routines-new"));
        // Editor renders with default cron `0 8 * * *`.
        await waitFor(() => {
            expect(screen.getByTestId("routine-cron")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("routine-name"), {
            target: { value: "morning" },
        });
        fireEvent.change(screen.getByTestId("routine-prompt"), {
            target: { value: "do the thing" },
        });
        // Wait for preview to land (debounced 250ms).
        await waitFor(
            () => {
                expect(
                    calls.some(
                        (c) =>
                            c.url === "/api/admin/routines/preview" &&
                            c.init?.method === "POST",
                    ),
                ).toBe(true);
            },
            { timeout: 2000 },
        );
        fireEvent.click(screen.getByTestId("routine-save"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/routines" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/routines" &&
                c.init?.method === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.name).toBe("morning");
        expect(body.schedule_cron).toBe("0 8 * * *");
        expect(body.prompt).toBe("do the thing");
    });

    it("Run-now POSTs and refreshes the list", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/routines") return listResponse([routine()]);
            if (
                url === "/api/admin/routines/r-1/run-now" &&
                init?.method === "POST"
            ) {
                return new Response(
                    JSON.stringify({
                        id: "run-1",
                        routine_id: "r-1",
                        fired_at: 1_700_000_500,
                        started_at: null,
                        finished_at: 1_700_000_501,
                        status: "Skipped",
                        error: "stub",
                        conversation_id: null,
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("routine-row")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("routine-run-now"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/routines/r-1/run-now" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
    });

    it("history toggle fetches runs and shows them", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/routines") return listResponse([routine()]);
            if (url.startsWith("/api/admin/routines/r-1/runs")) {
                return new Response(
                    JSON.stringify({
                        runs: [
                            {
                                id: "run-1",
                                routine_id: "r-1",
                                fired_at: 1_700_000_500,
                                started_at: null,
                                finished_at: 1_700_000_501,
                                status: "Skipped",
                                error: "scheduler stub",
                                conversation_id: null,
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
            expect(screen.getByTestId("routine-row")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("routine-history"));
        await waitFor(() => {
            expect(
                screen.getByTestId("routine-history-row"),
            ).toBeInTheDocument();
        });
        expect(screen.getByText(/scheduler stub/)).toBeInTheDocument();
    });

    it("delete confirms then drops the row", async () => {
        const confirmSpy = vi
            .spyOn(window, "confirm")
            .mockImplementation(() => true);
        let deleted = false;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/routines/r-1" &&
                init?.method === "DELETE"
            ) {
                deleted = true;
                return new Response("", { status: 200 });
            }
            if (url === "/api/admin/routines") {
                return deleted
                    ? listResponse([])
                    : listResponse([routine()]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("routine-row")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("routine-delete"));
        await waitFor(() => {
            expect(screen.getByTestId("routines-empty")).toBeInTheDocument();
        });
        confirmSpy.mockRestore();
    });
});
