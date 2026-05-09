import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { AppBoot } from "../routes/AppBoot";
import { AuthProvider } from "../auth/AuthContext";

function renderAppBoot() {
    return render(
        <AuthProvider>
            <MemoryRouter initialEntries={["/"]}>
                <Routes>
                    <Route path="/" element={<AppBoot />} />
                    <Route
                        path="/setup"
                        element={<div data-testid="route">setup-page</div>}
                    />
                    <Route
                        path="/login"
                        element={<div data-testid="route">login-page</div>}
                    />
                    <Route
                        path="/chat"
                        element={<div data-testid="route">chat-page</div>}
                    />
                </Routes>
            </MemoryRouter>
        </AuthProvider>,
    );
}

describe("AppBoot routing", () => {
    let fetchMock: ReturnType<typeof vi.fn>;

    beforeEach(() => {
        fetchMock = vi.fn();
        vi.stubGlobal("fetch", fetchMock);
    });

    afterEach(() => {
        vi.unstubAllGlobals();
    });

    it("ping=setup → routes to /setup", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("setup", { status: 200 }),
        );
        renderAppBoot();
        await waitFor(() => {
            expect(screen.getByTestId("route")).toHaveTextContent("setup-page");
        });
    });

    it("ping=pong + no stored tokens → routes to /login", async () => {
        fetchMock.mockResolvedValueOnce(
            new Response("pong", { status: 200 }),
        );
        renderAppBoot();
        await waitFor(() => {
            expect(screen.getByTestId("route")).toHaveTextContent("login-page");
        });
    });

    it("ping network failure → renders the boot-error screen", async () => {
        fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
        renderAppBoot();
        await waitFor(() => {
            expect(screen.getByTestId("boot-error-detail")).toBeInTheDocument();
        });
    });

    it("stored tokens that resolve to /me → routes straight to /chat", async () => {
        localStorage.setItem("execlaw.access_token", "a");
        localStorage.setItem("execlaw.refresh_token", "r");
        // The auth bootstrap and the ping probe race in parallel; key
        // by URL rather than call order so either ordering works.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") {
                return new Response(
                    JSON.stringify({
                        user_id: "controller-1",
                        username: "jlong",
                        display_name: "J",
                        email: null,
                        role: "controller",
                        last_login_at: null,
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/ping") {
                return new Response("pong", { status: 200 });
            }
            return new Response("not-mocked", { status: 500 });
        });
        renderAppBoot();
        await waitFor(() => {
            expect(screen.getByTestId("route")).toHaveTextContent("chat-page");
        });
    });
});
