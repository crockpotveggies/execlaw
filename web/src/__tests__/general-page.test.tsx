// Tests for the Settings → General page (Phase 14 bare-metal pivot).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { GeneralPage } from "../settings/GeneralPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = (role: "controller" | "operator" = "controller") =>
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

function settingsResponse(overrides: Partial<Record<string, unknown>> = {}) {
    return new Response(
        JSON.stringify({
            start_on_boot: true,
            bind_address: "127.0.0.1:3030",
            updated_at: 100,
            bind_address_requires_restart: true,
            ...overrides,
        }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <GeneralPage />
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

describe("GeneralPage", () => {
    it("loads + renders the seeded defaults", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") return settingsResponse();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("general-form")).toBeInTheDocument();
        });
        const startOnBoot = screen.getByTestId(
            "general-start-on-boot",
        ) as HTMLInputElement;
        const bindAddr = screen.getByTestId(
            "general-bind-address",
        ) as HTMLInputElement;
        expect(startOnBoot.checked).toBe(true);
        expect(bindAddr.value).toBe("127.0.0.1:3030");
    });

    it("disables Save until a field changes", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") return settingsResponse();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("general-save")).toBeInTheDocument();
        });
        const save = screen.getByTestId("general-save") as HTMLButtonElement;
        expect(save.disabled).toBe(true);
        // Toggle start_on_boot.
        fireEvent.click(screen.getByTestId("general-start-on-boot"));
        expect(save.disabled).toBe(false);
    });

    it("PUTs the changed bind_address and surfaces the restart hint", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/settings/general" &&
                init?.method === "PUT"
            ) {
                return settingsResponse({ bind_address: "0.0.0.0:9000" });
            }
            return settingsResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("general-form")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("general-bind-address"), {
            target: { value: "0.0.0.0:9000" },
        });
        // Restart hint appears as soon as the field is dirtied.
        expect(
            screen.getByTestId("general-bind-restart-hint"),
        ).toBeInTheDocument();
        fireEvent.click(screen.getByTestId("general-save"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/settings/general" &&
                        c.init?.method === "PUT",
                ),
            ).toBe(true);
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/settings/general" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.bind_address).toBe("0.0.0.0:9000");
        expect(body.start_on_boot).toBeUndefined(); // unchanged → omitted
    });

    it("operators see read-only — no Save button", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/settings/general") return settingsResponse();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/Only Controllers can change/i),
            ).toBeInTheDocument();
        });
        expect(screen.queryByTestId("general-save")).toBeNull();
    });

    it("surfaces server errors as an error banner", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") {
                // Mirrors `ApiError::into_response`'s wire shape:
                // `{error: {code, message}}`.
                return new Response(
                    JSON.stringify({
                        error: {
                            code: "invalid_bind_address",
                            message: "could not parse 'garbage' as host:port",
                        },
                    }),
                    { status: 400 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/could not parse/i),
            ).toBeInTheDocument();
        });
    });
});
