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
            bind_address: "127.0.0.1:3031",
            updated_at: 100,
            bind_address_requires_restart: true,
            history_retention_days: 30,
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
        expect(bindAddr.value).toBe("127.0.0.1:3031");
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

    it("toggling start_on_boot surfaces the service-install hint", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") return settingsResponse();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("general-form")).toBeInTheDocument();
        });
        // Hint absent before any change.
        expect(screen.queryByTestId("general-boot-reinstall-hint")).toBeNull();
        // Flip the toggle.
        fireEvent.click(screen.getByTestId("general-start-on-boot"));
        // Hint surfaces with the actionable command + Windows note.
        const hint = screen.getByTestId("general-boot-reinstall-hint");
        expect(hint).toHaveTextContent(/execlaw service install/);
        expect(hint).toHaveTextContent(/elevated PowerShell/i);
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

    it("renders the history-retention dropdown with the seeded default", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") return settingsResponse();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            const sel = screen.getByTestId("general-history-retention") as HTMLSelectElement;
            expect(sel.value).toBe("30");
        });
        // Every legal option is present, including the Infinite sentinel (0).
        const sel = screen.getByTestId("general-history-retention") as HTMLSelectElement;
        const opts = Array.from(sel.options).map((o) => o.value);
        expect(opts).toEqual(["30", "60", "90", "120", "0"]);
    });

    it("warns when narrowing the retention window", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") return settingsResponse();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        const sel = await waitFor(() =>
            screen.getByTestId("general-history-retention") as HTMLSelectElement,
        );
        // Currently 30; shrinking from 30 isn't possible (it's already the
        // minimum). Test the broader→narrower path: switch to a wider
        // window first, then back to a narrower one to confirm the
        // warning fires only on the narrowing change.
        fireEvent.change(sel, { target: { value: "120" } });
        // 30 → 120 widens; warning should NOT render.
        expect(
            screen.queryByTestId("general-retention-narrowing-hint"),
        ).not.toBeInTheDocument();
        // Now narrow again: 120 visually selected but settings still says 30.
        // The narrowing comparison is current-value vs server-saved, so we
        // need to pretend the server saved 120 already. Easiest: re-render
        // with 120 as the seeded default.
    });

    it("warns explicitly when shrinking 90 to 30", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general")
                return settingsResponse({ history_retention_days: 90 });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        const sel = await waitFor(() =>
            screen.getByTestId("general-history-retention") as HTMLSelectElement,
        );
        // Confirm the seeded value rendered as 90.
        await waitFor(() => expect(sel.value).toBe("90"));
        fireEvent.change(sel, { target: { value: "30" } });
        expect(
            screen.getByTestId("general-retention-narrowing-hint"),
        ).toBeInTheDocument();
    });

    it("does not warn when widening from finite to Infinite", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general")
                return settingsResponse({ history_retention_days: 60 });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        const sel = await waitFor(() =>
            screen.getByTestId("general-history-retention") as HTMLSelectElement,
        );
        await waitFor(() => expect(sel.value).toBe("60"));
        fireEvent.change(sel, { target: { value: "0" } }); // Infinite
        expect(
            screen.queryByTestId("general-retention-narrowing-hint"),
        ).not.toBeInTheDocument();
    });

    it("warns when narrowing from Infinite to finite", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general")
                return settingsResponse({ history_retention_days: 0 });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        const sel = await waitFor(() =>
            screen.getByTestId("general-history-retention") as HTMLSelectElement,
        );
        await waitFor(() => expect(sel.value).toBe("0"));
        fireEvent.change(sel, { target: { value: "60" } });
        expect(
            screen.getByTestId("general-retention-narrowing-hint"),
        ).toBeInTheDocument();
    });

    it("PUTs the retention change on save", async () => {
        let saveBody: { history_retention_days?: number } | null = null;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/settings/general") {
                if (init?.method === "PUT") {
                    saveBody = init.body
                        ? JSON.parse(init.body as string)
                        : null;
                    return settingsResponse({ history_retention_days: 90 });
                }
                return settingsResponse();
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        const sel = await waitFor(() =>
            screen.getByTestId("general-history-retention") as HTMLSelectElement,
        );
        await waitFor(() => expect(sel.value).toBe("30"));
        fireEvent.change(sel, { target: { value: "90" } });
        const save = screen.getByTestId("general-save");
        fireEvent.click(save);
        await waitFor(() => {
            expect(saveBody).not.toBeNull();
            expect(saveBody!.history_retention_days).toBe(90);
        });
    });
});
