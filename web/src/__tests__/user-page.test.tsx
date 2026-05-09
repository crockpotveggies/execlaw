// Tests for Settings → User (Phase 8.7 rename of the consolidated
// Login page).
//
// Covers the four sections: operator accounts, change password,
// passkeys, sessions.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { UserPage } from "../settings/UserPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = (role: "controller" | "operator" | "viewer" = "controller") =>
    new Response(
        JSON.stringify({
            user_id: "ctrl-1",
            username: "ctrl",
            display_name: "Ctrl",
            email: null,
            role,
            last_login_at: null,
        }),
        { status: 200 },
    );

function mountPage() {
    return render(
        <MemoryRouter>
            <AuthProvider>
                <UserPage />
            </AuthProvider>
        </MemoryRouter>,
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

describe("UserPage — change password section", () => {
    it("rejects mismatched confirmation locally without calling the API", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/webauthn/credentials")
                return new Response(JSON.stringify({ credentials: [] }), {
                    status: 200,
                });
            if (url === "/api/admin/users")
                return new Response(JSON.stringify({ users: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("user-password-card"),
            ).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("user-password-current"), {
            target: { value: "current" },
        });
        fireEvent.change(screen.getByTestId("user-password-new"), {
            target: { value: "longer-than-8" },
        });
        fireEvent.change(screen.getByTestId("user-password-confirm"), {
            target: { value: "different" },
        });
        fireEvent.click(screen.getByTestId("user-password-submit"));
        await waitFor(() => {
            expect(
                screen.getByText(/don't match/i),
            ).toBeInTheDocument();
        });
        // No PATCH/POST to the password endpoint.
        expect(
            calls.some((c) => c.url === "/api/admin/me/password"),
        ).toBe(false);
    });

    it("posts current + new password when valid", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/webauthn/credentials")
                return new Response(JSON.stringify({ credentials: [] }), {
                    status: 200,
                });
            if (url === "/api/admin/users")
                return new Response(JSON.stringify({ users: [] }), {
                    status: 200,
                });
            if (url === "/api/admin/me/password" && init?.method === "POST") {
                return new Response(JSON.stringify({ ok: true }), {
                    status: 200,
                });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("user-password-card"),
            ).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("user-password-current"), {
            target: { value: "hunter2-longer" },
        });
        fireEvent.change(screen.getByTestId("user-password-new"), {
            target: { value: "newer-passphrase-1" },
        });
        fireEvent.change(screen.getByTestId("user-password-confirm"), {
            target: { value: "newer-passphrase-1" },
        });
        fireEvent.click(screen.getByTestId("user-password-submit"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/me/password" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/me/password" &&
                c.init?.method === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.current_password).toBe("hunter2-longer");
        expect(body.new_password).toBe("newer-passphrase-1");
    });
});

describe("UserPage — operator accounts section", () => {
    it("renders the user list with a 'you' marker on the current user", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/users")
                return new Response(
                    JSON.stringify({
                        users: [
                            {
                                user_id: "ctrl-1",
                                username: "ctrl",
                                display_name: "Ctrl",
                                email: null,
                                role: "controller",
                                created_at: 0,
                                last_login_at: null,
                            },
                            {
                                user_id: "op-1",
                                username: "alice",
                                display_name: "Alice",
                                email: null,
                                role: "operator",
                                created_at: 0,
                                last_login_at: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            if (url === "/api/admin/webauthn/credentials")
                return new Response(JSON.stringify({ credentials: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("user-user-row")).toHaveLength(2);
        });
        expect(screen.getByText("you")).toBeInTheDocument();
    });

    it("controller can reset another user's password via the inline form", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/users" &&
                (init?.method ?? "GET") === "GET"
            ) {
                return new Response(
                    JSON.stringify({
                        users: [
                            {
                                user_id: "ctrl-1",
                                username: "ctrl",
                                display_name: "Ctrl",
                                email: null,
                                role: "controller",
                                created_at: 0,
                                last_login_at: null,
                            },
                            {
                                user_id: "op-1",
                                username: "alice",
                                display_name: "Alice",
                                email: null,
                                role: "operator",
                                created_at: 0,
                                last_login_at: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            if (
                url === "/api/admin/users/op-1/password" &&
                init?.method === "POST"
            ) {
                return new Response(JSON.stringify({ ok: true }), {
                    status: 200,
                });
            }
            if (url === "/api/admin/webauthn/credentials")
                return new Response(JSON.stringify({ credentials: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("user-user-row")).toHaveLength(2);
        });
        // Click the operator row's Reset password button.
        fireEvent.click(screen.getByTestId("user-reset-password"));
        fireEvent.change(screen.getByTestId("user-reset-password-input"), {
            target: { value: "operator-pass-2" },
        });
        fireEvent.click(screen.getByTestId("user-reset-submit"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/users/op-1/password" &&
                        c.init?.method === "POST",
                ),
            ).toBe(true);
        });
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/users/op-1/password" &&
                c.init?.method === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.new_password).toBe("operator-pass-2");
    });

    it("operators see a read-only view (no invite, no delete, no reset)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/users")
                return new Response(
                    JSON.stringify({
                        users: [
                            {
                                user_id: "ctrl-1",
                                username: "ctrl",
                                display_name: "Ctrl",
                                email: null,
                                role: "controller",
                                created_at: 0,
                                last_login_at: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            if (url === "/api/admin/webauthn/credentials")
                return new Response(JSON.stringify({ credentials: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("user-user-row")).toHaveLength(1);
        });
        expect(screen.queryByTestId("user-invite")).toBeNull();
        expect(screen.queryByTestId("user-user-delete")).toBeNull();
        expect(screen.queryByTestId("user-reset-password")).toBeNull();
    });
});

describe("UserPage — sessions section", () => {
    it("renders a Sign-out-everywhere button", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/webauthn/credentials")
                return new Response(JSON.stringify({ credentials: [] }), {
                    status: 200,
                });
            if (url === "/api/admin/users")
                return new Response(JSON.stringify({ users: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("user-sign-out-everywhere"),
            ).toBeInTheDocument();
        });
    });
});
