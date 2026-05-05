// Tests for the Settings → Plugin → Signal config page (Phase 8).
//
// Covers:
//   * Loading state until /api/admin/signal/status resolves.
//   * Not-paired branch fetches the QR PNG and renders it via a
//     blob URL once the upstream sidecar returns 200.
//   * Not-paired branch surfaces the upstream error (e.g. signal-cli
//     can't reach Signal because of TLS interception) instead of
//     showing a blank <img>.
//   * Paired branch renders the registered number + Unlink button
//     and confirms before firing DELETE.
//   * Sidecar-not-running state surfaces the "waiting for sidecar"
//     hint and skips the QR fetch.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SignalConfigPage } from "../settings/SignalConfigPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

// jsdom doesn't ship URL.createObjectURL — stub it so the
// component's blob-URL handoff resolves to a stable string we can
// assert on.
const blobUrl = "blob:mock://qr";
const createObjectURL = vi.fn(() => blobUrl);
const revokeObjectURL = vi.fn();

const meResponse = () =>
    new Response(
        JSON.stringify({
            user_id: "controller-1",
            username: "ctrl",
            display_name: "Ctrl",
            email: null,
            role: "controller",
            last_login_at: null,
        }),
        { status: 200 },
    );

interface StatusFixture {
    sidecar_status: string;
    sidecar_rpc_url: string | null;
    registered_accounts: string[];
    fetch_error: string | null;
}

function statusResponse(s: StatusFixture) {
    return new Response(JSON.stringify(s), { status: 200 });
}

function mountPage() {
    return render(
        <AuthProvider>
            <SignalConfigPage
                pluginId="signal"
                pluginVersion="0.1.0"
            />
        </AuthProvider>,
    );
}

beforeEach(() => {
    localStorage.setItem("execlaw.access_token", "tok");
    localStorage.setItem("execlaw.refresh_token", "tok");
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    createObjectURL.mockClear();
    revokeObjectURL.mockClear();
    // jsdom 24 doesn't define these — stub so the component can
    // hand the fetched blob to <img src>.
    (globalThis.URL as unknown as { createObjectURL: typeof createObjectURL }).createObjectURL =
        createObjectURL;
    (globalThis.URL as unknown as { revokeObjectURL: typeof revokeObjectURL }).revokeObjectURL =
        revokeObjectURL;
});

afterEach(() => {
    vi.unstubAllGlobals();
});

function pngResponse() {
    // 1x1 transparent PNG bytes — body content is irrelevant since
    // we only assert the blob handoff happened.
    const bytes = new Uint8Array([
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
    ]);
    return new Response(bytes, {
        status: 200,
        headers: { "content-type": "image/png" },
    });
}

describe("SignalConfigPage", () => {
    it("renders the QR pairing block when no accounts are registered", async () => {
        const qrCalls: string[] = [];
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: [],
                    fetch_error: null,
                });
            }
            if (url.startsWith("/api/admin/signal/qrcodelink")) {
                qrCalls.push(url);
                return pngResponse();
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-pairing-block"),
            ).toBeInTheDocument();
        });
        // QR is fetched (not <img src>) so the auth-bearing query-
        // string URL still flows through fetch and an upstream
        // failure can be surfaced as a real error message rather
        // than a blank image. The fetched URL must still carry the
        // bearer-token fallback for the access-token-aware proxy.
        await waitFor(() => {
            expect(qrCalls.length).toBeGreaterThan(0);
        });
        const qrUrl = qrCalls[0];
        expect(qrUrl).toContain("/api/admin/signal/qrcodelink");
        expect(qrUrl).toContain("device_name=execlaw");
        expect(qrUrl).toContain("access_token=tok");
        // PNG body → <img> renders the blob URL once the fetch
        // resolves.
        await waitFor(() => {
            const qr = screen.getByTestId(
                "signal-pairing-qr",
            ) as HTMLImageElement;
            expect(qr.src).toBe(blobUrl);
        });
        // Paired block does NOT render in this state.
        expect(screen.queryByTestId("signal-paired-block")).toBeNull();
    });

    it("surfaces the upstream sidecar error when QR generation fails", async () => {
        // Real failure mode: bbernhard's signal-cli was returning
        // `{"error":"Couldn't create QR code: no data to encode"}`
        // (signal-cli's link command couldn't reach Signal's
        // bootstrap servers — old image version, network outage,
        // etc.). Pre-fix, the SPA loaded this 400 response into a
        // bare <img src> which silently fell back to a blank
        // placeholder. The fix routes the QR through fetch and
        // shows the actual error.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: [],
                    fetch_error: null,
                });
            }
            if (url.startsWith("/api/admin/signal/qrcodelink")) {
                return new Response(
                    JSON.stringify({
                        code: "sidecar_qr_failed",
                        message:
                            "signal-cli /v1/qrcodelink returned HTTP 400: Couldn't create QR code: no data to encode",
                    }),
                    {
                        status: 502,
                        headers: { "content-type": "application/json" },
                    },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-pairing-qr-error"),
            ).toBeInTheDocument();
        });
        const errBlock = screen.getByTestId("signal-pairing-qr-error");
        expect(errBlock.textContent).toContain("no data to encode");
        // QR <img> doesn't render when the upstream failed.
        expect(screen.queryByTestId("signal-pairing-qr")).toBeNull();
    });

    it("renders the paired block + unlink button when an account is registered", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: ["+15551234567"],
                    fetch_error: null,
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-paired-block"),
            ).toBeInTheDocument();
        });
        expect(screen.getByText("+15551234567")).toBeInTheDocument();
        // QR pairing block does NOT render.
        expect(screen.queryByTestId("signal-pairing-block")).toBeNull();
    });

    it("unlink confirms then DELETEs the account", async () => {
        const confirmSpy = vi
            .spyOn(window, "confirm")
            .mockImplementation(() => true);
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/signal/accounts/%2B15551234567" &&
                init?.method === "DELETE"
            ) {
                return new Response("", { status: 204 });
            }
            if (url === "/api/admin/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: ["+15551234567"],
                    fetch_error: null,
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText("+15551234567")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("signal-paired-unlink"));
        // Confirm fires; DELETE flushes.
        await waitFor(() => {
            const del = calls.find(
                (c) =>
                    c.url ===
                        "/api/admin/signal/accounts/%2B15551234567" &&
                    c.init?.method === "DELETE",
            );
            expect(del).toBeDefined();
        });
        expect(confirmSpy).toHaveBeenCalledTimes(1);
        confirmSpy.mockRestore();
    });

    it("shows the waiting hint when the sidecar isn't running yet", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/signal/status") {
                return statusResponse({
                    sidecar_status: "starting",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    fetch_error: null,
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-pairing-waiting"),
            ).toBeInTheDocument();
        });
        // QR <img> doesn't render until the sidecar publishes a
        // host port.
        expect(screen.queryByTestId("signal-pairing-qr")).toBeNull();
        // Status chip reflects the supervisor's state.
        expect(
            screen.getByTestId("signal-sidecar-status").textContent,
        ).toBe("starting");
    });

    it("surfaces a /v1/accounts fetch error verbatim", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: [],
                    fetch_error: "signal-cli /v1/accounts returned HTTP 500",
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-sidecar-fetch-error"),
            ).toBeInTheDocument();
        });
        expect(
            screen.getByTestId("signal-sidecar-fetch-error").textContent,
        ).toContain("HTTP 500");
    });
});
