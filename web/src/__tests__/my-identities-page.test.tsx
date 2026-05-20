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

interface TransportFixture {
    id: string;
    label: string;
    plugin_id?: string;
    handle_placeholder: string;
}

const DEFAULT_TRANSPORTS: TransportFixture[] = [
    {
        id: "signal",
        label: "Signal",
        plugin_id: "signal",
        handle_placeholder: "+15551234",
    },
];

function transportsResponse(transports: TransportFixture[] = DEFAULT_TRANSPORTS) {
    return new Response(JSON.stringify({ transports }), { status: 200 });
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
            if (url === "/api/admin/me/transports") return transportsResponse();
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
            if (url === "/api/admin/me/transports") return transportsResponse();
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
        // Signal is the default — only entry in the list now that
        // built-ins are gone.
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
            if (url === "/api/admin/me/transports") return transportsResponse();
            if (
                url === "/api/admin/me/identifiers/signal/%2B15551234" &&
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

    // --- Dynamic transport list (2026-05-04) ----------------------

    it("populates dropdown options from /api/admin/me/transports", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/me/identifiers") return listResponse([]);
            if (url === "/api/admin/me/transports") {
                return transportsResponse([
                    {
                        id: "signal",
                        label: "Signal",
                        plugin_id: "signal",
                        handle_placeholder: "+15551234",
                    },
                    {
                        id: "whatsapp",
                        label: "Whatsapp",
                        plugin_id: "whatsapp",
                        handle_placeholder: "+15551234",
                    },
                ]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("my-identities-empty"),
            ).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "my-identities-transport",
        ) as HTMLSelectElement;
        const opts = Array.from(select.options).map((o) => o.value);
        expect(opts).toEqual(["signal", "whatsapp"]);
        // No "(built-in)" chip — every entry is plugin-sourced now
        // that built-ins were retired.
        for (const opt of select.options) {
            expect(opt.textContent).not.toContain("(built-in)");
        }
    });

    it("shows a no-transports hint when the registry is empty", async () => {
        // Simulates a fresh install before any plugin lands AND
        // before the built-in fallback is wired — pure-empty case.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/me/identifiers") return listResponse([]);
            if (url === "/api/admin/me/transports") {
                return transportsResponse([]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("my-identities-no-transports"),
            ).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "my-identities-transport",
        ) as HTMLSelectElement;
        expect(select.disabled).toBe(true);
        const addBtn = screen.getByTestId("my-identities-add");
        expect((addBtn as HTMLButtonElement).disabled).toBe(true);
    });

    it("uses the transport's handle_placeholder as the input hint", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/me/identifiers") return listResponse([]);
            if (url === "/api/admin/me/transports") {
                return transportsResponse([
                    {
                        id: "signal",
                        label: "Signal",
                        plugin_id: "signal",
                        handle_placeholder: "+15551234",
                    },
                    {
                        id: "email",
                        label: "Email",
                        plugin_id: "email-plugin",
                        handle_placeholder: "you@example.com",
                    },
                ]);
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        // First entry is signal — placeholder is the phone shape.
        // Wait on the placeholder *value*, not just the element: the
        // input mounts immediately with the fallback "handle"
        // placeholder, then re-renders with the transport's hint once
        // `listAvailableTransports` resolves and the default-select
        // useEffect commits `transport === "signal"`. The earlier
        // `waitFor(getByTestId)` form races that re-render on slow
        // CI runners.
        const handle = await waitFor(() => {
            const el = screen.getByTestId(
                "my-identities-handle",
            ) as HTMLInputElement;
            expect(el.placeholder).toBe("+15551234");
            return el;
        });
        fireEvent.change(screen.getByTestId("my-identities-transport"), {
            target: { value: "email" },
        });
        await waitFor(() => {
            expect(handle.placeholder).toBe("you@example.com");
        });
    });
});
