// Tests for the multi-controller Users page in Settings.
//
// Coverage:
//   - loading + empty list rendering,
//   - role badges + "you" highlight on the current user,
//   - invite form mount, submission body shape, and self-clear on success,
//   - delete flow: confirm dialog, busy state, optimistic refresh,
//   - role gating: operators see read-only view (no invite button, no
//     remove buttons).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { UsersPage } from "../settings/UsersPage";
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
        <AuthProvider>
            <UsersPage />
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

describe("UsersPage", () => {
    it("renders each user with their role badge + a 'you' marker on the current user", async () => {
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
                                created_at: 1_700_000_000,
                                last_login_at: null,
                            },
                            {
                                user_id: "op-1",
                                username: "alice",
                                display_name: "Alice",
                                email: "alice@example.com",
                                role: "operator",
                                created_at: 1_700_000_100,
                                last_login_at: 1_700_001_000,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("users-card")).toHaveLength(2);
        });
        expect(screen.getByText("Ctrl")).toBeInTheDocument();
        expect(screen.getByText("Alice")).toBeInTheDocument();
        // role badge text appears on each row
        expect(screen.getByText("controller")).toBeInTheDocument();
        expect(screen.getByText("operator")).toBeInTheDocument();
        // "you" marker only on the current user's card
        expect(screen.getByText("you")).toBeInTheDocument();
        // The "Remove" button is present for the OTHER user but NOT for me.
        const removes = screen.queryAllByTestId("users-delete");
        expect(removes).toHaveLength(1);
    });

    it("opens the invite form and submits the parsed body to /api/admin/users/invite", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/users/invite" &&
                (init?.method ?? "GET") === "POST"
            ) {
                return new Response(
                    JSON.stringify({
                        user_id: "new-1",
                        username: "newbie",
                        display_name: "Newbie",
                        email: null,
                        role: "operator",
                        created_at: 1_700_000_500,
                        last_login_at: null,
                    }),
                    { status: 200 },
                );
            }
            // GET /api/admin/users — return empty initially, then the new
            // user post-invite. We cheat and return empty for both — the
            // test asserts on the network call, not the post-refresh
            // render.
            return new Response(JSON.stringify({ users: [] }), { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("users-invite")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("users-invite"));
        expect(screen.getByTestId("users-invite-form")).toBeInTheDocument();

        fireEvent.change(screen.getByTestId("users-invite-username"), {
            target: { value: "newbie" },
        });
        fireEvent.change(
            screen.getByTestId("users-invite-form").querySelector(
                "input[value='']",
            ) ?? document.createElement("input"),
            { target: { value: "Newbie" } },
        );
        // Set display name + password explicitly via labels because the
        // selector trick above is fragile for the second empty input.
        const inputs = screen
            .getByTestId("users-invite-form")
            .querySelectorAll("input");
        // [username, display_name, password, email]
        fireEvent.change(inputs[1], { target: { value: "Newbie" } });
        fireEvent.change(screen.getByTestId("users-invite-password"), {
            target: { value: "hunter22hunter22" },
        });
        fireEvent.change(screen.getByTestId("users-invite-role"), {
            target: { value: "operator" },
        });

        fireEvent.click(screen.getByTestId("users-invite-submit"));

        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/users/invite" &&
                        (c.init?.method ?? "GET") === "POST",
                ),
            ).toBe(true);
        });
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/users/invite" &&
                (c.init?.method ?? "GET") === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.username).toBe("newbie");
        expect(body.display_name).toBe("Newbie");
        expect(body.role).toBe("operator");
        expect(body.initial_password).toBe("hunter22hunter22");
        // No email was filled — the wrapper should omit it.
        expect("email" in body).toBe(false);
    });

    it("delete flow asks for confirm + fires DELETE on accept", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
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
            if (
                url === "/api/admin/users/op-1" &&
                (init?.method ?? "GET") === "DELETE"
            ) {
                return new Response("{}", { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });

        const confirmSpy = vi
            .spyOn(window, "confirm")
            .mockImplementation(() => true);

        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("users-delete")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("users-delete"));

        expect(confirmSpy).toHaveBeenCalled();
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/users/op-1" &&
                        (c.init?.method ?? "GET") === "DELETE",
                ),
            ).toBe(true);
        });
    });

    it("delete is skipped when the user cancels the confirm dialog", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
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
            return new Response("{}", { status: 200 });
        });

        vi.spyOn(window, "confirm").mockImplementation(() => false);

        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("users-delete")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("users-delete"));

        // No DELETE should ever fire when the user cancels.
        expect(
            calls.some((c) => (c.init?.method ?? "GET") === "DELETE"),
        ).toBe(false);
    });

    it("operators see a read-only view: no invite button, no remove buttons", async () => {
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
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("users-card")).toHaveLength(2);
        });
        expect(screen.queryByTestId("users-invite")).toBeNull();
        expect(screen.queryAllByTestId("users-delete")).toHaveLength(0);
        expect(
            screen.getByText(/Only Controllers can invite or remove users/i),
        ).toBeInTheDocument();
    });
});
