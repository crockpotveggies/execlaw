import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../api/client";
import { installPlugin, ping, postLogin, postSetup } from "../api/endpoints";

describe("endpoints", () => {
    let fetchMock: ReturnType<typeof vi.fn>;

    beforeEach(() => {
        fetchMock = vi.fn();
        vi.stubGlobal("fetch", fetchMock);
    });

    afterEach(() => {
        vi.unstubAllGlobals();
    });

    describe("ping", () => {
        it("returns 'setup' verbatim", async () => {
            fetchMock.mockResolvedValueOnce(new Response("setup", { status: 200 }));
            await expect(ping()).resolves.toBe("setup");
        });

        it("returns 'pong' verbatim", async () => {
            fetchMock.mockResolvedValueOnce(new Response("pong", { status: 200 }));
            await expect(ping()).resolves.toBe("pong");
        });

        it("rejects unknown text bodies with ApiError(unknown)", async () => {
            fetchMock.mockResolvedValueOnce(
                new Response("ohno", { status: 200 }),
            );
            try {
                await ping();
                throw new Error("should have thrown");
            } catch (e) {
                const err = e as ApiError;
                expect(err.code).toBe("unknown");
            }
        });
    });

    describe("postSetup", () => {
        it("POSTs JSON with username + display_name + password and returns the token pair", async () => {
            fetchMock.mockResolvedValueOnce(
                new Response(
                    JSON.stringify({
                        principal_id: "controller-x",
                        access_token: "a",
                        refresh_token: "r",
                    }),
                    { status: 200 },
                ),
            );
            const out = await postSetup({
                username: "jlong",
                admin_password: "hunter2-longer",
                display_name: "Justin",
            });
            expect(out.principal_id).toBe("controller-x");
            const [url, init] = fetchMock.mock.calls[0];
            expect(url).toBe("/api/setup");
            expect((init as RequestInit).method).toBe("POST");
            expect(JSON.parse((init as RequestInit).body as string)).toEqual({
                username: "jlong",
                admin_password: "hunter2-longer",
                display_name: "Justin",
            });
        });
    });

    describe("postLogin", () => {
        it("posts username + password", async () => {
            fetchMock.mockResolvedValueOnce(
                new Response(
                    JSON.stringify({ access_token: "a", refresh_token: "r" }),
                    { status: 200 },
                ),
            );
            await postLogin({
                username: "jlong",
                admin_password: "hunter2-longer",
            });
            const [, init] = fetchMock.mock.calls[0];
            expect(JSON.parse((init as RequestInit).body as string)).toEqual({
                username: "jlong",
                admin_password: "hunter2-longer",
            });
        });

        it("surfaces 401 as ApiError(unauthorized) with bad_credentials code", async () => {
            fetchMock.mockResolvedValueOnce(
                new Response(
                    JSON.stringify({
                        error: { code: "bad_credentials", message: "nope" },
                    }),
                    { status: 401 },
                ),
            );
            try {
                await postLogin({ username: "jlong", admin_password: "wrong" });
                throw new Error("should have thrown");
            } catch (e) {
                const err = e as ApiError;
                expect(err.code).toBe("unauthorized");
                expect(err.serverCode).toBe("bad_credentials");
            }
        });
    });

    describe("installPlugin", () => {
        // jsdom's File doesn't implement arrayBuffer(); shim it for the
        // purposes of these tests.
        function fakeFile(bytes: number[], name = "p.zip"): File {
            const file = new File([new Uint8Array(bytes)], name, {
                type: "application/zip",
            });
            Object.defineProperty(file, "arrayBuffer", {
                value: async () => new Uint8Array(bytes).buffer,
                configurable: true,
            });
            return file;
        }

        it("posts the file body as application/zip", async () => {
            fetchMock.mockResolvedValueOnce(
                new Response(
                    JSON.stringify({ plugin_id: "alpha", version: "1.0.0" }),
                    { status: 200 },
                ),
            );
            const file = fakeFile([0x50, 0x4b, 0x03, 0x04]);
            const r = await installPlugin(file, () => "tok");
            expect(r.plugin_id).toBe("alpha");
            const [url, init] = fetchMock.mock.calls[0];
            expect(url).toBe("/api/admin/plugins/install");
            const headers = (init as RequestInit).headers as Record<
                string,
                string
            >;
            expect(headers["content-type"]).toBe("application/zip");
            expect(headers.authorization).toBe("Bearer tok");
        });

        it("surfaces 401 as ApiError(unauthorized)", async () => {
            fetchMock.mockResolvedValueOnce(
                new Response(
                    JSON.stringify({
                        error: { code: "invalid_token", message: "no" },
                    }),
                    { status: 401 },
                ),
            );
            const file = fakeFile([0x50]);
            await expect(
                installPlugin(file, () => null),
            ).rejects.toBeInstanceOf(ApiError);
        });
    });
});
