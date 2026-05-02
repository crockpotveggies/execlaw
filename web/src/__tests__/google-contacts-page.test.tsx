// Tests for the Settings → Google Contacts page (Phase 9).
//
// Three rendering states are exercised:
//   1. Not configured (404 from get_client_handler) → form open with
//      default redirect URI pre-filled.
//   2. Configured but not connected → "Connect Account" button +
//      collapsed credentials form.
//   3. Connected → "Connected as <email>" + Refresh / Disconnect.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { GoogleContactsPage } from "../settings/GoogleContactsPage";
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

function mountPage() {
    return render(
        <AuthProvider>
            <GoogleContactsPage
                pluginId="google-contacts"
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
    vi.restoreAllMocks();
});

describe("GoogleContactsPage", () => {
    it("renders the not-configured form with default redirect URI on a 404", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url ===
                "/api/admin/oauth/clients/google-contacts/controller"
            ) {
                return new Response(
                    JSON.stringify({
                        error: "oauth_not_found",
                        message: "no oauth client",
                    }),
                    { status: 404 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("client-id-input")).toBeInTheDocument();
        });
        const redirectInput = screen.getByTestId(
            "redirect-uri-input",
        ) as HTMLInputElement;
        // jsdom's window.location.origin is "http://localhost:3000" by
        // default; the page should pre-fill the callback URL using it.
        expect(redirectInput.value).toContain("/api/oauth/google/callback");
        // Save button is disabled until both client_id + redirect URI
        // are non-empty.
        expect(screen.getByTestId("save-button")).toBeDisabled();
    });

    it("renders 'configured but not connected' state when tokens are absent", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url ===
                "/api/admin/oauth/clients/google-contacts/controller"
            ) {
                return new Response(
                    JSON.stringify({
                        plugin_id: "google-contacts",
                        account_name: "controller",
                        provider: "google",
                        client_id: "abc.apps.googleusercontent.com",
                        redirect_uri:
                            "http://localhost:3030/api/oauth/google/callback",
                        scopes: [
                            "https://www.googleapis.com/auth/contacts.readonly",
                        ],
                        created_at: 100,
                        updated_at: 100,
                        connected: false,
                        account_email: null,
                        token_expires_at: null,
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("google-contacts-needs-connect"),
            ).toBeInTheDocument();
        });
        expect(screen.getByTestId("connect-button")).toBeInTheDocument();
        // Pre-populated form: client_id round-trips, secret is blank
        // (sentinel for preserve-on-save).
        const clientIdInput = screen.getByTestId(
            "client-id-input",
        ) as HTMLInputElement;
        expect(clientIdInput.value).toBe("abc.apps.googleusercontent.com");
        const secretInput = screen.getByTestId(
            "client-secret-input",
        ) as HTMLInputElement;
        expect(secretInput.value).toBe("");
    });

    it("renders connected state with email and disconnect button", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url ===
                "/api/admin/oauth/clients/google-contacts/controller"
            ) {
                return new Response(
                    JSON.stringify({
                        plugin_id: "google-contacts",
                        account_name: "controller",
                        provider: "google",
                        client_id: "abc.apps.googleusercontent.com",
                        redirect_uri:
                            "http://localhost:3030/api/oauth/google/callback",
                        scopes: [
                            "https://www.googleapis.com/auth/contacts.readonly",
                        ],
                        created_at: 100,
                        updated_at: 100,
                        connected: true,
                        account_email: "alice@example.com",
                        token_expires_at: 9_999_999_999,
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("google-contacts-connected"),
            ).toBeInTheDocument();
        });
        expect(screen.getByText(/alice@example\.com/)).toBeInTheDocument();
        expect(screen.getByText(/Disconnect/i)).toBeInTheDocument();
        // The "needs connect" card must not also be present — the
        // states are mutually exclusive.
        expect(
            screen.queryByTestId("google-contacts-needs-connect"),
        ).toBeNull();
    });

    it("PUTs the upsert request when Save is clicked", async () => {
        let putBody: Record<string, unknown> | null = null;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url ===
                    "/api/admin/oauth/clients/google-contacts/controller" &&
                (init?.method ?? "GET") === "GET"
            ) {
                return new Response(
                    JSON.stringify({
                        error: "oauth_not_found",
                        message: "no oauth client",
                    }),
                    { status: 404 },
                );
            }
            if (
                url ===
                    "/api/admin/oauth/clients/google-contacts/controller" &&
                init?.method === "PUT"
            ) {
                putBody = JSON.parse(init.body as string);
                return new Response(
                    JSON.stringify({
                        plugin_id: "google-contacts",
                        account_name: "controller",
                        provider: "google",
                        client_id: putBody!.client_id,
                        redirect_uri: putBody!.redirect_uri,
                        scopes: putBody!.scopes,
                        created_at: 100,
                        updated_at: 200,
                        connected: false,
                        account_email: null,
                        token_expires_at: null,
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("client-id-input")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("client-id-input"), {
            target: { value: "test.apps.googleusercontent.com" },
        });
        fireEvent.change(screen.getByTestId("client-secret-input"), {
            target: { value: "GOCSPX-test" },
        });
        fireEvent.change(screen.getByTestId("redirect-uri-input"), {
            target: { value: "http://localhost:3030/api/oauth/google/callback" },
        });
        fireEvent.click(screen.getByTestId("save-button"));
        await waitFor(() => {
            expect(putBody).not.toBeNull();
        });
        expect(putBody!).toMatchObject({
            provider: "google",
            client_id: "test.apps.googleusercontent.com",
            client_secret: "GOCSPX-test",
            redirect_uri: "http://localhost:3030/api/oauth/google/callback",
        });
        expect(putBody!.scopes).toEqual(
            expect.arrayContaining([
                "https://www.googleapis.com/auth/contacts.readonly",
            ]),
        );
        // After save, the page transitions to the configured-but-not-
        // connected state.
        await waitFor(() => {
            expect(
                screen.getByTestId("google-contacts-needs-connect"),
            ).toBeInTheDocument();
        });
    });

    it("opens the authorize URL when Connect Account is clicked", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url ===
                    "/api/admin/oauth/clients/google-contacts/controller" &&
                (init?.method ?? "GET") === "GET"
            ) {
                return new Response(
                    JSON.stringify({
                        plugin_id: "google-contacts",
                        account_name: "controller",
                        provider: "google",
                        client_id: "abc",
                        redirect_uri: "http://x",
                        scopes: ["x"],
                        created_at: 0,
                        updated_at: 0,
                        connected: false,
                        account_email: null,
                        token_expires_at: null,
                    }),
                    { status: 200 },
                );
            }
            if (
                url ===
                    "/api/admin/oauth/clients/google-contacts/controller/connect" &&
                init?.method === "POST"
            ) {
                return new Response(
                    JSON.stringify({
                        authorize_url:
                            "https://accounts.google.com/o/oauth2/v2/auth?state=abc",
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        const openSpy = vi.fn();
        vi.stubGlobal("open", openSpy);
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("connect-button")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("connect-button"));
        await waitFor(() => {
            expect(openSpy).toHaveBeenCalled();
        });
        const args = openSpy.mock.calls[0];
        expect(args[0]).toBe(
            "https://accounts.google.com/o/oauth2/v2/auth?state=abc",
        );
        expect(args[1]).toBe("_blank");
    });
});
