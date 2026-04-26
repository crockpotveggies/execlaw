// Tests for the Settings → Trust policy page (Phase 9.2, §2.6).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { TrustPolicyPage } from "../settings/TrustPolicyPage";
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

function policy(
    overrides: Partial<{
        auto_trust_contacts: boolean;
        min_trust_hint_for_auto_trust: string;
        identity_plugin_order: string[];
        delegated_trust_default_ttl: string;
    }> = {},
) {
    return {
        auto_trust_contacts: overrides.auto_trust_contacts ?? true,
        min_trust_hint_for_auto_trust:
            overrides.min_trust_hint_for_auto_trust ?? "Contact",
        mixed_trust_policy: "min_wins",
        identity_plugin_order: overrides.identity_plugin_order ?? [],
        delegated_trust_default_ttl:
            overrides.delegated_trust_default_ttl ?? "7d",
    };
}

function mountPage() {
    return render(
        <AuthProvider>
            <TrustPolicyPage />
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

describe("TrustPolicyPage", () => {
    it("loads documented defaults into the form", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/trust-policy")
                return new Response(JSON.stringify(policy()), { status: 200 });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("trust-min-hint"),
            ).toBeInTheDocument();
        });
        expect(
            (screen.getByTestId("trust-min-hint") as HTMLSelectElement).value,
        ).toBe("Contact");
        expect(
            (screen.getByTestId("trust-ttl") as HTMLInputElement).value,
        ).toBe("7d");
        expect(
            (screen.getByTestId("trust-auto-toggle") as HTMLInputElement)
                .checked,
        ).toBe(true);
    });

    it("PUTs the form, including the plugin-order array round-trip", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/trust-policy" && init?.method === "PUT") {
                return new Response(
                    JSON.stringify(
                        policy({
                            auto_trust_contacts: false,
                            identity_plugin_order: ["signal", "google"],
                            delegated_trust_default_ttl: "12h",
                        }),
                    ),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/trust-policy")
                return new Response(JSON.stringify(policy()), { status: 200 });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("trust-save")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("trust-auto-toggle"));
        fireEvent.change(screen.getByTestId("trust-ttl"), {
            target: { value: "12h" },
        });
        fireEvent.change(screen.getByTestId("trust-plugin-order"), {
            target: { value: "signal\ngoogle" },
        });
        fireEvent.click(screen.getByTestId("trust-save"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/trust-policy" &&
                        c.init?.method === "PUT",
                ),
            ).toBe(true);
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/trust-policy" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.auto_trust_contacts).toBe(false);
        expect(body.delegated_trust_default_ttl).toBe("12h");
        expect(body.identity_plugin_order).toEqual(["signal", "google"]);
    });

    it("rejects bad TTL locally without sending", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/trust-policy")
                return new Response(JSON.stringify(policy()), { status: 200 });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("trust-save")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("trust-ttl"), {
            target: { value: "yesterday" },
        });
        fireEvent.click(screen.getByTestId("trust-save"));
        await waitFor(() => {
            expect(
                screen.getByText(/Delegated TTL must look like/i),
            ).toBeInTheDocument();
        });
        expect(
            calls.some(
                (c) =>
                    c.url === "/api/admin/trust-policy" &&
                    c.init?.method === "PUT",
            ),
        ).toBe(false);
    });
});
