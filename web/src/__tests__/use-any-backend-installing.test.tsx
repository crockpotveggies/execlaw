import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { useAnyBackendInstalling } from "../chat/useAnyBackendInstalling";
import type { BackendStatusResponse } from "../api/endpoints";

// Hook tests for the brand-indicator's "is-installing" data source.
//
// We stub the global `fetch` (the only way `getBackendStatus` reaches
// the network) so each test can dictate what every per-purpose status
// call returns. The hook is wired to BACKEND_PURPOSES, which has 4
// entries, so each test renders one fetch call per purpose.

function backendStatus(
    overrides: Partial<BackendStatusResponse>,
): BackendStatusResponse {
    return {
        purpose: "Standard",
        mode: "managed",
        status: "Stopped",
        endpoint: null,
        restart_attempts: 0,
        supervisor_available: true,
        stage: "Idle",
        elapsed_secs: null,
        last_log_line: null,
        download_progress: null,
        ...overrides,
    };
}

function jsonResponse(body: unknown): Response {
    return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
    });
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

const token = () => "t";

describe("useAnyBackendInstalling", () => {
    it("returns false when every backend is idle / external", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url.includes("/status")) {
                return jsonResponse(backendStatus({}));
            }
            return new Response("{}", { status: 200 });
        });
        const { result } = renderHook(() =>
            useAnyBackendInstalling(token, true),
        );
        // The hook fires an initial poll; wait one tick for the
        // resulting setState to flush before asserting.
        await waitFor(() => {
            expect(fetchMock).toHaveBeenCalled();
        });
        // Stays false even after the polls land.
        await new Promise((r) => setTimeout(r, 10));
        expect(result.current).toBe(false);
    });

    it("flips to true when any backend reports status = Pulling", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url.includes("Standard/status")) {
                return jsonResponse(
                    backendStatus({ status: "Pulling", stage: "PullingImage" }),
                );
            }
            if (url.includes("/status")) {
                return jsonResponse(backendStatus({}));
            }
            return new Response("{}", { status: 200 });
        });
        const { result } = renderHook(() =>
            useAnyBackendInstalling(token, true),
        );
        await waitFor(() => {
            expect(result.current).toBe(true);
        });
    });

    it("flips to true when status = Starting and stage = LoadingModel", async () => {
        // Common Ollama path: daemon is up (Starting), model is
        // loading to GPU (LoadingModel). Both signals overlap; the
        // hook OR's them so either alone triggers the indicator.
        fetchMock.mockImplementation(async (url: string) => {
            if (url.includes("Small/status")) {
                return jsonResponse(
                    backendStatus({
                        purpose: "Small",
                        status: "Starting",
                        stage: "LoadingModel",
                    }),
                );
            }
            if (url.includes("/status")) {
                return jsonResponse(backendStatus({}));
            }
            return new Response("{}", { status: 200 });
        });
        const { result } = renderHook(() =>
            useAnyBackendInstalling(token, true),
        );
        await waitFor(() => {
            expect(result.current).toBe(true);
        });
    });

    it("ignores rows where supervisor_available is false", async () => {
        // Stopped backends with the supervisor unavailable shouldn't
        // count — that's the dev-build / Docker-unreachable case.
        // The indicator would otherwise pulse forever on a Linux
        // workstation that has no Docker installed.
        fetchMock.mockImplementation(async (url: string) => {
            if (url.includes("/status")) {
                return jsonResponse(
                    backendStatus({
                        status: "Pulling",
                        stage: "PullingImage",
                        supervisor_available: false,
                    }),
                );
            }
            return new Response("{}", { status: 200 });
        });
        const { result } = renderHook(() =>
            useAnyBackendInstalling(token, true),
        );
        await waitFor(() => {
            expect(fetchMock).toHaveBeenCalled();
        });
        await new Promise((r) => setTimeout(r, 10));
        expect(result.current).toBe(false);
    });

    it("treats network errors as not installing", async () => {
        // 401 / 500 / connection refused — the indicator must stay
        // calm rather than misreporting an install. The
        // ConnectionBanner / disconnected-state indicator covers the
        // actual outage signal.
        fetchMock.mockImplementation(async () => {
            throw new Error("boom");
        });
        const { result } = renderHook(() =>
            useAnyBackendInstalling(token, true),
        );
        await waitFor(() => {
            expect(fetchMock).toHaveBeenCalled();
        });
        await new Promise((r) => setTimeout(r, 10));
        expect(result.current).toBe(false);
    });

    it("does nothing when not enabled (unauthenticated app boot)", async () => {
        renderHook(() => useAnyBackendInstalling(token, false));
        await new Promise((r) => setTimeout(r, 10));
        expect(fetchMock).not.toHaveBeenCalled();
    });
});
