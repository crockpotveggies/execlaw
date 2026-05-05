// Tests for the Settings → Plugin → Signal config page (Phase 8).
//
// Covers:
//   * Loading state until /api/admin/signal/status resolves.
//   * Not-paired branch renders the QR-code <img> with the
//     bearer-token query-string fallback in the src attribute.
//   * Paired branch renders the registered number + Unlink button
//     and confirms before firing DELETE.
//   * Sidecar-not-running state surfaces the "waiting for sidecar"
//     hint and skips the QR <img>.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SignalConfigPage } from "../settings/SignalConfigPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

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
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("SignalConfigPage", () => {
    it("renders the QR pairing block when no accounts are registered", async () => {
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
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-pairing-block"),
            ).toBeInTheDocument();
        });
        const qr = screen.getByTestId("signal-pairing-qr") as HTMLImageElement;
        // Bearer-token query-string fallback so the raw <img>
        // load can authenticate without an Authorization header.
        expect(qr.src).toContain("/api/admin/signal/qrcodelink");
        expect(qr.src).toContain("device_name=execlaw");
        expect(qr.src).toContain("access_token=tok");
        // Paired block does NOT render in this state.
        expect(screen.queryByTestId("signal-paired-block")).toBeNull();
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
