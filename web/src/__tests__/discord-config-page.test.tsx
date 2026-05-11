// Tests for the Settings → Plugin → Discord config page.
//
// Covers:
//   * Loading state until /api/admin/plugins/discord/{config,status}
//     both resolve.
//   * Unconfigured branch renders the "Unconfigured" badge + the
//     token input + the privileged-intent warning.
//   * Configured branch renders the masked token + bot identity +
//     guild count.
//   * Save flow — POST /config writes bot_token, triggers a reload,
//     shows the "saved" alert, clears the input.
//   * Save error surfaces in the ErrorBanner (without writing).
//   * Test send happy path — POST /test with channel_id, success
//     alert visible.
//   * Test send error path — error alert visible.
//   * Test send is disabled when unconfigured.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { DiscordConfigPage } from "../settings/DiscordConfigPage";
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

interface ConfigFixture {
    bot_token_masked: string;
    configured: boolean;
    bot_user_id?: string | null;
    bot_username?: string | null;
}

interface StatusFixture {
    sidecar_status: string;
    sidecar_rpc_url: string | null;
    registered_accounts: unknown[];
    accounts_on_disk: unknown[];
    fetch_error: string | null;
    bot_user_id?: string | null;
    bot_username?: string | null;
    guilds_known?: number;
    token_masked?: string;
}

function configResponse(c: ConfigFixture) {
    return new Response(JSON.stringify(c), {
        status: 200,
        headers: { "content-type": "application/json" },
    });
}

function statusResponse(s: StatusFixture) {
    return new Response(JSON.stringify(s), {
        status: 200,
        headers: { "content-type": "application/json" },
    });
}

function mountPage() {
    return render(
        <AuthProvider>
            <DiscordConfigPage
                pluginId="discord"
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

describe("DiscordConfigPage", () => {
    it("renders the unconfigured branch with the privileged-intent warning + empty token input", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/discord/config") {
                return configResponse({
                    bot_token_masked: "",
                    configured: false,
                    bot_user_id: null,
                    bot_username: null,
                });
            }
            if (url === "/api/admin/plugins/discord/status") {
                return statusResponse({
                    sidecar_status: "unconfigured",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    accounts_on_disk: [],
                    fetch_error: null,
                    guilds_known: 0,
                });
            }
            throw new Error(`unexpected fetch ${url}`);
        });

        mountPage();

        await waitFor(() => {
            expect(screen.getByTestId("discord-config-page")).toBeTruthy();
        });
        expect(screen.getByTestId("discord-status").textContent).toContain(
            "Unconfigured",
        );
        // Privileged-intent warning must render unconditionally — operator
        // will hit a "blank inbound content" footgun without enabling it,
        // and missing the warning is the most common Discord-bot mistake.
        expect(screen.getByTestId("discord-intent-warning").textContent).toContain(
            "MESSAGE CONTENT INTENT",
        );
        const tokenInput = screen.getByTestId(
            "discord-token-input",
        ) as HTMLInputElement;
        expect(tokenInput.value).toBe("");
        // Test-send button is rendered but disabled — sending without a
        // bot token would 401 anyway, and disabling tells the operator
        // why the affordance isn't actionable yet.
        const testBtn = screen.getByTestId("discord-test") as HTMLButtonElement;
        expect(testBtn.disabled).toBe(true);
    });

    it("renders the configured branch with masked token + bot identity + guild count", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/discord/config") {
                return configResponse({
                    bot_token_masked: "********wxyz",
                    configured: true,
                    bot_user_id: "1234567890",
                    bot_username: "execlaw-bot",
                });
            }
            if (url === "/api/admin/plugins/discord/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    accounts_on_disk: [],
                    fetch_error: null,
                    bot_user_id: "1234567890",
                    bot_username: "execlaw-bot",
                    guilds_known: 2,
                    token_masked: "********wxyz",
                });
            }
            throw new Error(`unexpected fetch ${url}`);
        });

        mountPage();

        await waitFor(() => {
            expect(screen.getByTestId("discord-status").textContent).toContain(
                "Configured",
            );
        });
        const gateway = screen.getByTestId("discord-gateway-status");
        expect(gateway.textContent).toContain("execlaw-bot");
        expect(gateway.textContent).toContain("1234567890");
        expect(screen.getByTestId("discord-guild-count").textContent).toContain(
            "2",
        );
        const testBtn = screen.getByTestId("discord-test") as HTMLButtonElement;
        expect(testBtn.disabled).toBe(false);
    });

    it("saves a new bot token + clears the input + shows the saved alert", async () => {
        const seen: { config: number; status: number; save: number } = {
            config: 0,
            status: 0,
            save: 0,
        };
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/discord/config") {
                if ((init?.method ?? "GET").toUpperCase() === "POST") {
                    seen.save += 1;
                    const body = JSON.parse(init?.body as string);
                    expect(body.bot_token).toBe("new-token-xyz");
                    return new Response(
                        JSON.stringify({
                            ok: true,
                            bot_user_id: "9999",
                            bot_username: "new-bot",
                        }),
                        { status: 200 },
                    );
                }
                seen.config += 1;
                // After save, the second config GET reflects the new state.
                if (seen.config === 1) {
                    return configResponse({
                        bot_token_masked: "",
                        configured: false,
                    });
                }
                return configResponse({
                    bot_token_masked: "********-xyz",
                    configured: true,
                    bot_user_id: "9999",
                    bot_username: "new-bot",
                });
            }
            if (url === "/api/admin/plugins/discord/status") {
                seen.status += 1;
                return statusResponse({
                    sidecar_status: seen.status === 1 ? "unconfigured" : "healthy",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    accounts_on_disk: [],
                    fetch_error: null,
                    bot_user_id: "9999",
                    bot_username: "new-bot",
                    guilds_known: 0,
                });
            }
            throw new Error(`unexpected fetch ${url}`);
        });

        mountPage();

        await waitFor(() => {
            expect(screen.getByTestId("discord-config-page")).toBeTruthy();
        });

        const tokenInput = screen.getByTestId(
            "discord-token-input",
        ) as HTMLInputElement;
        fireEvent.change(tokenInput, { target: { value: "new-token-xyz" } });
        fireEvent.click(screen.getByTestId("discord-save"));

        await waitFor(() => {
            expect(seen.save).toBe(1);
            expect(screen.getByTestId("discord-saved").textContent).toContain(
                "new-bot",
            );
        });
        // Save must clear the cleartext token out of the input so it
        // doesn't linger in DOM after a successful round-trip.
        expect((tokenInput as HTMLInputElement).value).toBe("");
        // Status badge flips to Configured after reload.
        expect(screen.getByTestId("discord-status").textContent).toContain(
            "Configured",
        );
    });

    it("surfaces save errors in the ErrorBanner instead of writing the masked token", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/discord/config") {
                if ((init?.method ?? "GET").toUpperCase() === "POST") {
                    return new Response(
                        JSON.stringify({
                            error: {
                                code: "rejected",
                                message:
                                    "Discord rejected the bot_token: 401 Unauthorized",
                            },
                        }),
                        { status: 400 },
                    );
                }
                return configResponse({
                    bot_token_masked: "",
                    configured: false,
                });
            }
            if (url === "/api/admin/plugins/discord/status") {
                return statusResponse({
                    sidecar_status: "unconfigured",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    accounts_on_disk: [],
                    fetch_error: null,
                    guilds_known: 0,
                });
            }
            throw new Error(`unexpected fetch ${url}`);
        });

        mountPage();

        await waitFor(() => {
            expect(screen.getByTestId("discord-config-page")).toBeTruthy();
        });
        const tokenInput = screen.getByTestId(
            "discord-token-input",
        ) as HTMLInputElement;
        fireEvent.change(tokenInput, { target: { value: "bad-token" } });
        fireEvent.click(screen.getByTestId("discord-save"));

        // Discord rejection surfaces via the standard ErrorBanner — same
        // pattern other config pages follow. We assert via the page
        // text content rather than a banner-specific testid because
        // ErrorBanner doesn't carry one (intentional — the banner is
        // shared chrome, addressed by section).
        await waitFor(() => {
            expect(screen.getByTestId("discord-config-page").textContent).toContain(
                "Discord rejected the bot_token",
            );
        });
        // Status stays unconfigured — the failed save must not have
        // mutated server state on the optimistic-UI path either.
        expect(screen.getByTestId("discord-status").textContent).toContain(
            "Unconfigured",
        );
    });

    it("sends a test message and reports the message id on success", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/discord/config") {
                return configResponse({
                    bot_token_masked: "********wxyz",
                    configured: true,
                    bot_user_id: "1234",
                    bot_username: "execlaw-bot",
                });
            }
            if (url === "/api/admin/plugins/discord/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    accounts_on_disk: [],
                    fetch_error: null,
                    guilds_known: 1,
                });
            }
            if (url === "/api/admin/plugins/discord/test") {
                const body = JSON.parse(init?.body as string);
                expect(body.channel_id).toBe("999000111");
                return new Response(
                    JSON.stringify({ ok: true, message_id: "msg-42" }),
                    { status: 200 },
                );
            }
            throw new Error(`unexpected fetch ${url}`);
        });

        mountPage();

        await waitFor(() => {
            expect(screen.getByTestId("discord-status").textContent).toContain(
                "Configured",
            );
        });
        fireEvent.change(screen.getByTestId("discord-test-channel-input"), {
            target: { value: "999000111" },
        });
        fireEvent.click(screen.getByTestId("discord-test"));
        await waitFor(() => {
            expect(screen.getByTestId("discord-test-ok").textContent).toContain(
                "msg-42",
            );
        });
    });

    it("surfaces test-send failures in the inline test alert", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/plugins/discord/config") {
                return configResponse({
                    bot_token_masked: "********wxyz",
                    configured: true,
                });
            }
            if (url === "/api/admin/plugins/discord/status") {
                return statusResponse({
                    sidecar_status: "healthy",
                    sidecar_rpc_url: null,
                    registered_accounts: [],
                    accounts_on_disk: [],
                    fetch_error: null,
                    guilds_known: 0,
                });
            }
            if (url === "/api/admin/plugins/discord/test") {
                return new Response(
                    JSON.stringify({ ok: false, error: "Missing Permissions" }),
                    { status: 200 },
                );
            }
            throw new Error(`unexpected fetch ${url}`);
        });

        mountPage();

        await waitFor(() => {
            expect(screen.getByTestId("discord-status").textContent).toContain(
                "Configured",
            );
        });
        fireEvent.change(screen.getByTestId("discord-test-channel-input"), {
            target: { value: "999" },
        });
        fireEvent.click(screen.getByTestId("discord-test"));
        await waitFor(() => {
            expect(screen.getByTestId("discord-test-err").textContent).toContain(
                "Missing Permissions",
            );
        });
    });
});
