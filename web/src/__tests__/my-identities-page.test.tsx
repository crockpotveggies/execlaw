// Tests for the Settings → My Identities page (Phase 9.3, §7.1).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MyIdentitiesPage } from "../settings/MyIdentitiesPage";
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

function listResponse(
    identifiers: Array<{ transport: string; handle: string }> = [],
) {
    return new Response(
        JSON.stringify({
            controller_principal_id: "controller-1",
            identifiers,
        }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <MyIdentitiesPage />
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

describe("MyIdentitiesPage", () => {
    it("renders the empty hint when no identifiers exist", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/me/identifiers") return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("my-identities-empty"),
            ).toBeInTheDocument();
        });
    });

    it("posts add request and refreshes the list", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        let added = false;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/me/identifiers" &&
                init?.method === "POST"
            ) {
                added = true;
                return listResponse([
                    { transport: "signal", handle: "+15551234" },
                ]);
            }
            if (url === "/api/admin/me/identifiers") {
                return added
                    ? listResponse([
                          { transport: "signal", handle: "+15551234" },
                      ])
                    : listResponse([]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("my-identities-empty"),
            ).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("my-identities-handle"), {
            target: { value: "+15551234" },
        });
        fireEvent.click(screen.getByTestId("my-identities-add"));
        await waitFor(() => {
            expect(screen.getByTestId("my-identities-row")).toBeInTheDocument();
        });
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/me/identifiers" &&
                c.init?.method === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.transport).toBe("signal");
        expect(body.handle).toBe("+15551234");
    });

    it("delete confirms then removes the row", async () => {
        const confirmSpy = vi
            .spyOn(window, "confirm")
            .mockImplementation(() => true);
        let deleted = false;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (
                url ===
                    "/api/admin/me/identifiers/signal/%2B15551234" &&
                init?.method === "DELETE"
            ) {
                deleted = true;
                return listResponse([]);
            }
            if (url === "/api/admin/me/identifiers") {
                return deleted
                    ? listResponse([])
                    : listResponse([
                          { transport: "signal", handle: "+15551234" },
                      ]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByText(/\+15551234/)).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("my-identities-delete"));
        await waitFor(() => {
            expect(
                screen.getByTestId("my-identities-empty"),
            ).toBeInTheDocument();
        });
        confirmSpy.mockRestore();
    });
});
