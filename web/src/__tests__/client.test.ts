import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, apiFetch, setRefreshHook } from "../api/client";

describe("apiFetch", () => {
    let fetchMock: ReturnType<typeof vi.fn>;

    beforeEach(() => {
        fetchMock = vi.fn();
        vi.stubGlobal("fetch", fetchMock);
    });

    afterEach(() => {
        vi.unstubAllGlobals();
    });

    it("parses 2xx JSON bodies", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response(JSON.stringify({ hello: "world" }), {
                status: 200,
                headers: { "content-type": "application/json" },
            }),
        );
        const out = await apiFetch<{ hello: string }>("/api/x");
        expect(out.hello).toBe("world");
    });

    it("attaches Bearer token from accessor", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("{}", { status: 200 }),
        );
        await apiFetch("/api/x", {}, () => "tok-123");
        const init = fetchMock.mock.calls[0][1] as RequestInit;
        const headers = init.headers as Record<string, string>;
        expect(headers.authorization).toBe("Bearer tok-123");
    });

    it("does not attach Authorization when no token is present", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("{}", { status: 200 }),
        );
        await apiFetch("/api/x");
        const init = fetchMock.mock.calls[0][1] as RequestInit;
        const headers = init.headers as Record<string, string>;
        expect(headers.authorization).toBeUndefined();
    });

    it("maps 401 to ApiError(unauthorized)", async () => {
        // Response bodies can only be read once, so build a fresh
        // Response on every fetch call.
        fetchMock.mockImplementation(
            async () =>
                new Response(
                    JSON.stringify({
                        error: { code: "invalid_token", message: "no good" },
                    }),
                    { status: 401 },
                ),
        );
        await expect(apiFetch("/api/x")).rejects.toBeInstanceOf(ApiError);
        try {
            await apiFetch("/api/x");
            throw new Error("should have thrown");
        } catch (e) {
            expect(e).toBeInstanceOf(ApiError);
            const err = e as ApiError;
            expect(err.code).toBe("unauthorized");
            expect(err.serverCode).toBe("invalid_token");
            expect(err.status).toBe(401);
        }
    });

    it("maps 409 to ApiError(conflict) and surfaces server code", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response(
                JSON.stringify({
                    error: {
                        code: "already_initialized",
                        message: "no good",
                    },
                }),
                { status: 409 },
            ),
        );
        try {
            await apiFetch("/api/setup");
            throw new Error("should have thrown");
        } catch (e) {
            const err = e as ApiError;
            expect(err.code).toBe("conflict");
            expect(err.serverCode).toBe("already_initialized");
        }
    });

    it("rawText: returns the raw text body for /api/ping", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("setup", {
                status: 200,
                headers: { "content-type": "text/plain" },
            }),
        );
        const out = await apiFetch<string>("/api/ping", { rawText: true });
        expect(out).toBe("setup");
    });

    it("network failure becomes ApiError(network)", async () => {
        fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
        try {
            await apiFetch("/api/x");
            throw new Error("should have thrown");
        } catch (e) {
            const err = e as ApiError;
            expect(err.code).toBe("network");
            expect(err.status).toBe(0);
        }
    });

    it("malformed JSON on a JSON-expecting response surfaces as server error", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("<html>oops</html>", { status: 200 }),
        );
        try {
            await apiFetch("/api/x");
            throw new Error("should have thrown");
        } catch (e) {
            const err = e as ApiError;
            expect(err.code).toBe("server");
        }
    });

    it("explicit accessToken override beats the accessor", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("{}", { status: 200 }),
        );
        await apiFetch(
            "/api/x",
            { accessToken: "explicit" },
            () => "from-accessor",
        );
        const init = fetchMock.mock.calls[0][1] as RequestInit;
        const headers = init.headers as Record<string, string>;
        expect(headers.authorization).toBe("Bearer explicit");
    });

    describe("silent retry on 401 (Phase 7 hardening)", () => {
        afterEach(() => {
            setRefreshHook(null);
        });

        it("calls the installed refresh hook on 401 + retries with the new token", async () => {
            // First call: 401. Second call (the retry): 200.
            const calls: Array<RequestInit | undefined> = [];
            fetchMock.mockImplementation(async (_url, init) => {
                calls.push(init as RequestInit);
                if (calls.length === 1) {
                    return new Response(
                        JSON.stringify({
                            error: { code: "invalid_token", message: "expired" },
                        }),
                        { status: 401 },
                    );
                }
                return new Response(JSON.stringify({ ok: true }), {
                    status: 200,
                });
            });
            const hook = vi.fn(async () => "fresh-token");
            setRefreshHook(hook);

            const out = await apiFetch<{ ok: boolean }>(
                "/api/x",
                {},
                () => "stale-token",
            );
            expect(out.ok).toBe(true);
            expect(hook).toHaveBeenCalledTimes(1);
            // First request used the stale token; the retry used the
            // fresh one returned by the hook.
            const stale = calls[0]?.headers as Record<string, string>;
            const fresh = calls[1]?.headers as Record<string, string>;
            expect(stale.authorization).toBe("Bearer stale-token");
            expect(fresh.authorization).toBe("Bearer fresh-token");
        });

        it("does NOT retry when the hook returns null (refresh failed)", async () => {
            fetchMock.mockImplementation(
                async () =>
                    new Response(
                        JSON.stringify({
                            error: { code: "invalid_token", message: "x" },
                        }),
                        { status: 401 },
                    ),
            );
            const hook = vi.fn(async () => null);
            setRefreshHook(hook);

            await expect(
                apiFetch("/api/x", {}, () => "stale"),
            ).rejects.toBeInstanceOf(ApiError);
            // Hook was tried once; original 401 propagates.
            expect(hook).toHaveBeenCalledTimes(1);
            // Only one fetch call — the retry path was skipped because
            // the hook returned null.
            expect(fetchMock).toHaveBeenCalledTimes(1);
        });

        it("does not loop: a 401 on the retry surfaces normally", async () => {
            fetchMock.mockImplementation(
                async () =>
                    new Response(
                        JSON.stringify({
                            error: { code: "invalid_token", message: "x" },
                        }),
                        { status: 401 },
                    ),
            );
            const hook = vi.fn(async () => "fresh");
            setRefreshHook(hook);

            await expect(
                apiFetch("/api/x", {}, () => "stale"),
            ).rejects.toBeInstanceOf(ApiError);
            // Hook called once, two fetches (original + retry), then
            // we give up. No infinite loop.
            expect(hook).toHaveBeenCalledTimes(1);
            expect(fetchMock).toHaveBeenCalledTimes(2);
        });

        it("explicit accessToken disables the auto-retry", async () => {
            // When the caller passed `accessToken` explicitly they're
            // managing the credential themselves — we should not
            // rotate it under them.
            fetchMock.mockImplementation(
                async () =>
                    new Response(
                        JSON.stringify({ error: { code: "x", message: "x" } }),
                        { status: 401 },
                    ),
            );
            const hook = vi.fn(async () => "fresh");
            setRefreshHook(hook);

            await expect(
                apiFetch("/api/x", { accessToken: "caller-controlled" }),
            ).rejects.toBeInstanceOf(ApiError);
            expect(hook).not.toHaveBeenCalled();
        });
    });
});
