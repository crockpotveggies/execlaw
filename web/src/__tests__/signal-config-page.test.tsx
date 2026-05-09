// Tests for the Settings → Plugin → Signal config page.
//
// Covers:
//   * Loading state until /api/admin/plugins/signal/status resolves.
//   * Not-paired branch fetches the QR PNG (as a JSON {data_url}
//     response from the plugin admin handler) and renders it via
//     <img src=data:image/png;base64,...>.
//   * Not-paired branch surfaces the upstream error (e.g. signal-cli
//     can't reach Signal because of TLS interception) instead of
//     showing a blank <img>.
//   * Paired branch renders the registered number + Unlink button
//     and confirms before firing DELETE /unregister-account?number=...
//   * Sidecar-not-running state surfaces the "waiting for sidecar"
//     hint and skips the QR fetch.
//   * Status fetch_error renders verbatim.

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
    accounts_on_disk?: string[];
    fetch_error: string | null;
}

function statusResponse(s: StatusFixture) {
    const body = { accounts_on_disk: s.accounts_on_disk ?? [], ...s };
    return new Response(JSON.stringify(body), { status: 200 });
}

function qrJsonResponse(dataUrl: string) {
    return new Response(
        JSON.stringify({ data_url: dataUrl, mime_type: "image/png" }),
        { status: 200, headers: { "content-type": "application/json" } },
    );
}

function qrErrorJsonResponse(message: string) {
    return new Response(JSON.stringify({ error: message }), {
        status: 200,
        headers: { "content-type": "application/json" },
    });
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
        const qrCalls: string[] = [];
        const fakeDataUrl =
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: [],
                    fetch_error: null,
                });
            }
            if (url.startsWith("/api/admin/plugins/signal/qrcodelink")) {
                qrCalls.push(url);
                return qrJsonResponse(fakeDataUrl);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("signal-pairing-block"),
            ).toBeInTheDocument();
        });
        await waitFor(() => {
            expect(qrCalls.length).toBeGreaterThan(0);
        });
        expect(qrCalls[0]).toContain("/api/admin/plugins/signal/qrcodelink");
        expect(qrCalls[0]).toContain("device_name=execlaw");
        // The plugin returns JSON {data_url}; the SPA pipes that
        // straight into <img src> — no blob URL involved.
        await waitFor(() => {
            const qr = screen.getByTestId(
                "signal-pairing-qr",
            ) as HTMLImageElement;
            expect(qr.src).toBe(fakeDataUrl);
        });
        expect(screen.queryByTestId("signal-paired-block")).toBeNull();
    });

    it("surfaces the upstream sidecar error when QR generation fails", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/signal/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: "http://127.0.0.1:8501",
                    registered_accounts: [],
                    fetch_error: null,
                });
            }
            if (url.startsWith("/api/admin/plugins/signal/qrcodelink")) {
                return qrErrorJsonResponse(
                    "signal-cli /v1/qrcodelink returned HTTP 400: Couldn't create QR code: no data to encode",
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
        expect(screen.queryByTestId("signal-pairing-qr")).toBeNull();
    });

    it("renders the paired block + unlink button when an account is registered", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/signal/status") {
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
                url.startsWith(
                    "/api/admin/plugins/signal/unregister-account",
                ) &&
                init?.method === "DELETE"
            ) {
                return new Response("", { status: 204 });
            }
            if (url === "/api/admin/plugins/signal/status") {
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
        await waitFor(() => {
            const del = calls.find(
                (c) =>
                    c.url.startsWith(
                        "/api/admin/plugins/signal/unregister-account",
                    ) && c.init?.method === "DELETE",
            );
            expect(del).toBeDefined();
            // Number is passed as a query param now (the plugin
            // admin dispatcher matches paths literally and doesn't
            // support `/accounts/{number}`-style path params).
            expect(del!.url).toContain("number=%2B15551234567");
        });
        expect(confirmSpy).toHaveBeenCalledTimes(1);
        confirmSpy.mockRestore();
    });

    it("shows the waiting hint when the sidecar isn't running yet", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/signal/status") {
                return statusResponse({
                    sidecar_status: "down",
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
        expect(screen.queryByTestId("signal-pairing-qr")).toBeNull();
        expect(
            screen.getByTestId("signal-sidecar-status").textContent,
        ).toBe("down");
    });

    it("surfaces a /v1/accounts fetch error verbatim", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/signal/status") {
                return statusResponse({
                    sidecar_status: "unhealthy",
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
