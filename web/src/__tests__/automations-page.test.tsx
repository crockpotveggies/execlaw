// Tests for the /automations landing page (M4b).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { AutomationsPage } from "../settings/AutomationsPage";
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

function metricsResponse(over: Partial<Record<string, number | null>> = {}) {
    return new Response(
        JSON.stringify({
            active_count: over.active_count ?? 0,
            runs_24h: over.runs_24h ?? 0,
            success_rate_24h:
                over.success_rate_24h === undefined ? null : over.success_rate_24h,
            untriaged_kinds_24h: over.untriaged_kinds_24h ?? 0,
        }),
        { status: 200 },
    );
}

function automationsResponse(rows: unknown[]) {
    return new Response(JSON.stringify(rows), { status: 200 });
}

function suggestionsResponse(rows: unknown[]) {
    return new Response(JSON.stringify(rows), { status: 200 });
}

function automation(id: string, overrides: Partial<{ enabled: boolean; name: string }> = {}) {
    return {
        id,
        name: overrides.name ?? `auto-${id}`,
        enabled: overrides.enabled ?? true,
        definition: {
            trigger: { kind: "webhook.received", when: null },
            nodes: [{ id: "end", kind: "Terminal", config: {} }],
            edges: [{ from: "trigger", to: "end", when: null }],
        },
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    };
}

function suggestion(id: string, source: string, count: number) {
    return {
        id,
        kind: "webhook.received",
        source,
        event_count: count,
        sample_event_ids: ["e1", "e2"],
        suggested_name: `Automate ${source} webhook`,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    };
}

/**
 * Route fetch responses by URL substring. Order-insensitive — the
 * page calls metrics/list/suggestions in parallel via Promise.all.
 */
function setupRoutedFetch(map: Record<string, () => Response>) {
    fetchMock.mockImplementation((url: string | URL | Request) => {
        const s =
            typeof url === "string"
                ? url
                : url instanceof URL
                  ? url.toString()
                  : url.url;
        for (const [key, fn] of Object.entries(map)) {
            if (s.includes(key)) return Promise.resolve(fn());
        }
        if (s.includes("/api/me")) return Promise.resolve(meResponse());
        return Promise.resolve(new Response(JSON.stringify({}), { status: 200 }));
    });
}

function mountPage() {
    return render(
        <MemoryRouter>
            <AuthProvider>
                <AutomationsPage />
            </AuthProvider>
        </MemoryRouter>,
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

describe("AutomationsPage", () => {
    it("renders empty-state when no automations exist", async () => {
        setupRoutedFetch({
            "/automations/metrics": () => metricsResponse(),
            "/automations/suggestions": () => suggestionsResponse([]),
            "/api/admin/automations": () => automationsResponse([]),
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("automations-empty")).toBeInTheDocument();
        });
        // The "+ New automation" CTA is always available.
        expect(screen.getByTestId("new-automation-btn")).toBeInTheDocument();
    });

    it("renders metric cards with values from the metrics endpoint", async () => {
        setupRoutedFetch({
            "/automations/metrics": () =>
                metricsResponse({
                    active_count: 7,
                    runs_24h: 42,
                    success_rate_24h: 0.953,
                    untriaged_kinds_24h: 3,
                }),
            "/automations/suggestions": () => suggestionsResponse([]),
            "/api/admin/automations": () => automationsResponse([]),
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("metric-active")).toHaveTextContent("7");
        });
        expect(screen.getByTestId("metric-runs")).toHaveTextContent("42");
        expect(screen.getByTestId("metric-success")).toHaveTextContent("95.3%");
        expect(screen.getByTestId("metric-untriaged")).toHaveTextContent("3");
    });

    it("renders the success-rate card as em-dash when there are no runs in the window", async () => {
        setupRoutedFetch({
            "/automations/metrics": () => metricsResponse({}),
            "/automations/suggestions": () => suggestionsResponse([]),
            "/api/admin/automations": () => automationsResponse([]),
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("metric-success")).toHaveTextContent("—");
        });
    });

    it("renders the suggestions section when sweep has produced rows", async () => {
        setupRoutedFetch({
            "/automations/metrics": () => metricsResponse(),
            "/automations/suggestions": () =>
                suggestionsResponse([suggestion("s-1", "webhook:ring", 23)]),
            "/api/admin/automations": () => automationsResponse([]),
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("suggestions-section")).toBeInTheDocument();
        });
        expect(screen.getByTestId("suggestion-s-1")).toHaveTextContent(
            "Automate webhook:ring webhook",
        );
        expect(
            screen.getByTestId("suggestion-s-1-review"),
        ).toBeInTheDocument();
        expect(
            screen.getByTestId("suggestion-s-1-dismiss"),
        ).toBeInTheDocument();
    });

    it("omits the suggestions section when there are no pending suggestions", async () => {
        setupRoutedFetch({
            "/automations/metrics": () => metricsResponse(),
            "/automations/suggestions": () => suggestionsResponse([]),
            "/api/admin/automations": () => automationsResponse([]),
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("automations-empty")).toBeInTheDocument();
        });
        expect(screen.queryByTestId("suggestions-section")).toBeNull();
    });

    it("renders one row per automation with toggle button", async () => {
        setupRoutedFetch({
            "/automations/metrics": () => metricsResponse({ active_count: 2 }),
            "/automations/suggestions": () => suggestionsResponse([]),
            "/api/admin/automations": () =>
                automationsResponse([
                    automation("a-1", { name: "alpha", enabled: true }),
                    automation("a-2", { name: "beta", enabled: false }),
                ]),
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("automations-table")).toBeInTheDocument();
        });
        expect(screen.getByTestId("automation-row-a-1")).toHaveTextContent("alpha");
        expect(screen.getByTestId("automation-row-a-2")).toHaveTextContent("beta");
        // Status badges reflect enabled flag.
        expect(screen.getByTestId("automation-row-a-1")).toHaveTextContent(
            "Enabled",
        );
        expect(screen.getByTestId("automation-row-a-2")).toHaveTextContent(
            "Disabled",
        );
        // Toggle buttons present per row.
        expect(
            screen.getByTestId("automation-a-1-toggle"),
        ).toBeInTheDocument();
        expect(
            screen.getByTestId("automation-a-2-toggle"),
        ).toBeInTheDocument();
    });
});
