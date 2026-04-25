// Combined smoke tests for the smaller settings pages: logs, eval
// flags, principals, audit. Each verifies:
//   - the empty-state renders when the API returns nothing,
//   - rendered rows reflect the API payload,
//   - filters update the request URL.
//
// (The Hardware section was inlined into BackendsPage in Phase 8.6
// — see backends-page.test.tsx for its coverage.)

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AuthProvider } from "../auth/AuthContext";
import { LogsPage } from "../settings/LogsPage";
import { EvalFlagsPage } from "../settings/EvalFlagsPage";
import { PrincipalsPage } from "../settings/PrincipalsPage";
import { ContactsPage } from "../settings/ContactsPage";
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

// ---- PrincipalsPage / ContactsPage ---------------------------------
//
// Both pages share PrincipalList — they read /api/admin/principals and
// only differ on which trust classes survive the filter. PrincipalsPage
// shows the "everything else" view (Controller/Delegated/Blocked/etc.),
// ContactsPage shows the curated address book (KnownTrusted / Limited /
// UnknownPending). Tests assert both halves of the split.

const principalsFixture = [
    {
        id: "ctrl-1",
        trust_class: "Controller",
        display_name: "Controller",
        first_seen: 0,
        last_seen: null,
        identifiers: [] as Array<{ transport: string; handle: string }>,
    },
    {
        id: "knwn-1",
        trust_class: "KnownTrusted",
        display_name: "Marge",
        first_seen: 0,
        last_seen: null,
        identifiers: [{ transport: "signal", handle: "+15551234" }],
    },
    {
        id: "lim-1",
        trust_class: "KnownLimited",
        display_name: "Bart",
        first_seen: 0,
        last_seen: null,
        identifiers: [],
    },
    {
        id: "pend-1",
        trust_class: "UnknownPending",
        display_name: "Stranger",
        first_seen: 0,
        last_seen: null,
        identifiers: [],
    },
    {
        id: "blk-1",
        trust_class: "Blocked",
        display_name: "Spam",
        first_seen: 0,
        last_seen: null,
        identifiers: [],
    },
];

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
                screen.getByText(/no system principals on file yet/i),
            ).toBeInTheDocument();
        });
    });

    it("shows only non-contact classes (Controller, Blocked, …)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(
                JSON.stringify({ principals: principalsFixture }),
                { status: 200 },
            );
        });
        mountWithAuth(<PrincipalsPage />);
        await waitFor(() => {
            expect(
                screen.queryAllByText("Controller").length,
            ).toBeGreaterThan(0);
        });
        // System / non-contact classes render here.
        expect(screen.getByText("Spam")).toBeInTheDocument();
        // Address-book classes are routed to ContactsPage instead.
        expect(screen.queryByText("Marge")).toBeNull();
        expect(screen.queryByText("Bart")).toBeNull();
        expect(screen.queryByText("Stranger")).toBeNull();
        // Nothing in the non-contact bucket is revokable in this fixture
        // (Controller and Blocked are both non-revokable).
        expect(screen.queryAllByTestId("principal-revoke")).toHaveLength(0);
    });
});

describe("ContactsPage", () => {
    it("shows the empty hint when no contacts exist", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/principals")
                return new Response(
                    JSON.stringify({
                        // Only a system principal — Contacts should still
                        // render the empty state because the filter drops it.
                        principals: [principalsFixture[0]],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountWithAuth(<ContactsPage />);
        await waitFor(() => {
            expect(
                screen.getByText(/no contacts yet/i),
            ).toBeInTheDocument();
        });
    });

    it("shows only address-book classes and a revoke button per trusted row", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(
                JSON.stringify({ principals: principalsFixture }),
                { status: 200 },
            );
        });
        mountWithAuth(<ContactsPage />);
        await waitFor(() => {
            expect(screen.getByText("Marge")).toBeInTheDocument();
        });
        expect(screen.getByText("Bart")).toBeInTheDocument();
        expect(screen.getByText("Stranger")).toBeInTheDocument();
        // System principals stay on the Principals page.
        expect(screen.queryByText("Spam")).toBeNull();
        // Revoke is offered for KnownTrusted + KnownLimited; UnknownPending
        // is not revokable yet (the controller hasn't promoted it).
        expect(screen.queryAllByTestId("principal-revoke")).toHaveLength(2);
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
