import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, apiFetch } from "../api/client";

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
});
