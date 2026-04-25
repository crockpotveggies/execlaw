// Combined smoke tests for the smaller settings pages: hardware,
// logs, eval flags, principals, audit. Each verifies:
//   - the empty-state renders when the API returns nothing,
//   - rendered rows reflect the API payload,
//   - filters update the request URL.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AuthProvider } from "../auth/AuthContext";
import { HardwarePage } from "../settings/HardwarePage";
import { LogsPage } from "../settings/LogsPage";
import { EvalFlagsPage } from "../settings/EvalFlagsPage";
import { PrincipalsPage } from "../settings/PrincipalsPage";
import { AuditPage } from "../settings/AuditPage";

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
    localStorage.setItem("execlaw.access_token", "tok");
    localStorage.setItem("execlaw.refresh_token", "tok");
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => {
    vi.unstubAllGlobals();
});

const meResponse = () =>
    new Response(
        JSON.stringify({
            user_id: "c1",
            username: "u",
            display_name: "U",
            email: null,
            role: "controller",
            last_login_at: null,
        }),
        { status: 200 },
    );

function mountWithAuth(ui: React.ReactElement) {
    return render(<AuthProvider>{ui}</AuthProvider>);
}

// ---- HardwarePage --------------------------------------------------

describe("HardwarePage", () => {
    it("shows the no-GPU notice when the profile has none", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(JSON.stringify({ gpus: [] }), { status: 200 });
        });
        mountWithAuth(<HardwarePage />);
        await waitFor(() => {
            expect(screen.getByText(/no gpus detected/i)).toBeInTheDocument();
        });
    });

    it("lists each GPU returned by the server", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(
                JSON.stringify({
                    gpus: [
                        {
                            vendor: "NVIDIA",
                            model: "RTX 4090",
                            pci_vendor_id: "10de",
                            pci_device_id: "2684",
                        },
                    ],
                }),
                { status: 200 },
            );
        });
        mountWithAuth(<HardwarePage />);
        await waitFor(() => {
            expect(screen.getByText(/GPUs \(1\)/)).toBeInTheDocument();
        });
        // The model name shows in both the friendly row + the raw JSON
        // dump; either occurrence proves the data made it through.
        expect(screen.getAllByText(/RTX 4090/).length).toBeGreaterThan(0);
    });
});

// ---- LogsPage ------------------------------------------------------

describe("LogsPage", () => {
    it("renders the empty state and the level filter", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/logs"))
                return new Response(JSON.stringify({ entries: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountWithAuth(<LogsPage />);
        await waitFor(() => {
            expect(screen.getByTestId("logs-level")).toBeInTheDocument();
        });
        expect(screen.getByText(/no log entries match/i)).toBeInTheDocument();
    });

    it("appends the level filter to the URL on change", async () => {
        const calls: string[] = [];
        fetchMock.mockImplementation(async (url: string) => {
            calls.push(url);
            if (url === "/api/admin/me") return meResponse();
            return new Response(JSON.stringify({ entries: [] }), {
                status: 200,
            });
        });
        mountWithAuth(<LogsPage />);
        await waitFor(() => {
            expect(screen.getByTestId("logs-level")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("logs-level"), {
            target: { value: "warn" },
        });
        await waitFor(() => {
            expect(
                calls.some((c) => c.includes("/api/admin/logs?level=warn")),
            ).toBe(true);
        });
    });

    it("renders log rows with level + message", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/logs"))
                return new Response(
                    JSON.stringify({
                        entries: [
                            {
                                ts_ms: 0,
                                level: "ERROR",
                                target: "execlaw_server::chats",
                                conversation_id: "c1",
                                plugin_id: null,
                                message: "boom",
                                fields: null,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountWithAuth(<LogsPage />);
        await waitFor(() => {
            expect(screen.getByText("ERROR")).toBeInTheDocument();
        });
        expect(screen.getByText("boom")).toBeInTheDocument();
    });
});

// ---- EvalFlagsPage -------------------------------------------------

describe("EvalFlagsPage", () => {
    it("shows the empty hint when no flags exist", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/eval/flags"))
                return new Response(JSON.stringify({ flags: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountWithAuth(<EvalFlagsPage />);
        await waitFor(() => {
            expect(
                screen.getByText(/no eval flags match/i),
            ).toBeInTheDocument();
        });
    });

    it("lists flags with their conversation id + label", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(
                JSON.stringify({
                    flags: [
                        {
                            id: 1,
                            label: "wrong_tool",
                            conversation_id: "conv-x",
                            seq: 4,
                            flagged_at: 0,
                            notes: null,
                        },
                    ],
                }),
                { status: 200 },
            );
        });
        mountWithAuth(<EvalFlagsPage />);
        await waitFor(() => {
            expect(screen.getByText("wrong_tool")).toBeInTheDocument();
        });
        expect(screen.getByText(/conv-x/)).toBeInTheDocument();
    });
});

// ---- PrincipalsPage ------------------------------------------------

describe("PrincipalsPage", () => {
    it("shows the empty hint when the store has no rows", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/principals")
                return new Response(JSON.stringify({ principals: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountWithAuth(<PrincipalsPage />);
        await waitFor(() => {
            expect(
                screen.getByText(/no principals on file yet/i),
            ).toBeInTheDocument();
        });
    });

    it("shows revoke button only for trusted classes", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(
                JSON.stringify({
                    principals: [
                        {
                            id: "ctrl-1",
                            trust_class: "Controller",
                            display_name: "Controller",
                            first_seen: 0,
                            last_seen: null,
                            identifiers: [],
                        },
                        {
                            id: "knwn-1",
                            trust_class: "KnownTrusted",
                            display_name: "Marge",
                            first_seen: 0,
                            last_seen: null,
                            identifiers: [
                                { transport: "signal", handle: "+15551234" },
                            ],
                        },
                        {
                            id: "blk-1",
                            trust_class: "Blocked",
                            display_name: "Spam",
                            first_seen: 0,
                            last_seen: null,
                            identifiers: [],
                        },
                    ],
                }),
                { status: 200 },
            );
        });
        mountWithAuth(<PrincipalsPage />);
        // "Controller" appears as both the display name and as the
        // trust-class badge — wait for ONE of those rather than asserting
        // a unique match.
        await waitFor(() => {
            expect(
                screen.queryAllByText("Controller").length,
            ).toBeGreaterThan(0);
        });
        const revokes = screen.queryAllByTestId("principal-revoke");
        expect(revokes).toHaveLength(1); // only KnownTrusted is revokable here
    });
});

// ---- AuditPage -----------------------------------------------------

describe("AuditPage", () => {
    it("shows the empty hint when no audit rows exist", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url.startsWith("/api/admin/audit"))
                return new Response(JSON.stringify({ entries: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountWithAuth(<AuditPage />);
        await waitFor(() => {
            expect(
                screen.getByText(/no audit entries yet/i),
            ).toBeInTheDocument();
        });
    });

    it("renders entries with actor + table_name", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(
                JSON.stringify({
                    entries: [
                        {
                            id: 1,
                            ts: 1_700_000_000,
                            actor: "controller-1",
                            table_name: "config_runner_deployments",
                            row_id: "row-x",
                            old_json: null,
                            new_json: { k: "v" },
                        },
                    ],
                }),
                { status: 200 },
            );
        });
        mountWithAuth(<AuditPage />);
        await waitFor(() => {
            expect(screen.getByText("controller-1")).toBeInTheDocument();
        });
        expect(
            screen.getByText("config_runner_deployments"),
        ).toBeInTheDocument();
    });
});
