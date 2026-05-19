// Tests for the chat-component registry + the open-meteo-flavoured
// renderers that ship in the SPA build.

import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { AuthContext } from "../auth/AuthContext";
import {
    detectChatComponent,
    getChatComponent,
    registerChatComponent,
} from "../chat/chatComponentRegistry";

// 2026-05-19 — the chart-inline download link resolves a signed
// URL on mount instead of using a raw JWT in the query string.
// Mock the helper so tests can assert the rendered URL.
vi.mock("../api/signedDownloadUrl", () => ({
    signDownloadUrl: vi.fn(
        async (path: string) =>
            `${path}?exp=9999999999&user=u-test&sig=deadbeef`,
    ),
}));

import "../chat/components/ChartInlineComponent";
import "../chat/components/WeatherCurrentComponent";
import "../chat/components/WeatherDailyComponent";

function fakeAuth(): React.ContextType<typeof AuthContext> {
    return {
        getAccessToken: () => "header.payload.signature",
    } as unknown as React.ContextType<typeof AuthContext>;
}

describe("detectChatComponent", () => {
    it("returns null for empty or non-JSON text", () => {
        expect(detectChatComponent("")).toBeNull();
        expect(detectChatComponent("hello world")).toBeNull();
        expect(detectChatComponent("not { json")).toBeNull();
    });

    it("returns null for JSON without the kind marker", () => {
        expect(detectChatComponent(`{"foo": 1}`)).toBeNull();
    });

    it("returns null for arrays even if they decode", () => {
        expect(detectChatComponent(`[1, 2, 3]`)).toBeNull();
    });

    it("picks up a chat_component_kind marker", () => {
        const r = detectChatComponent(
            JSON.stringify({ chat_component_kind: "chart", svg: "<svg/>" }),
        );
        expect(r?.kind).toBe("chart");
        expect(r?.data.svg).toBe("<svg/>");
    });
});

describe("chat-component registry", () => {
    afterEach(() => {
        // Nothing — registry intentionally has no reset. The shipped
        // renderers persist across tests; we just verify identity.
    });

    it("registers the chart renderer at module load", () => {
        const Chart = getChatComponent("chart");
        expect(Chart).toBeTruthy();
    });

    it("registers the weather_current and weather_daily renderers", () => {
        expect(getChatComponent("weather_current")).toBeTruthy();
        expect(getChatComponent("weather_daily")).toBeTruthy();
    });

    it("returns undefined for unknown kinds", () => {
        expect(getChatComponent("does_not_exist_kind")).toBeUndefined();
    });

    it("supports user-defined kinds via registerChatComponent", () => {
        function Stub() {
            return <span data-testid="stub-component">ok</span>;
        }
        registerChatComponent("__test_stub__", Stub);
        const C = getChatComponent("__test_stub__");
        expect(C).toBe(Stub);
    });
});

describe("ChartInline component", () => {
    it("inlines the host-rendered SVG verbatim", () => {
        const Chart = getChatComponent("chart")!;
        render(
            <Chart
                data={{
                    chat_component_kind: "chart",
                    svg: '<svg xmlns="http://www.w3.org/2000/svg" data-testid="inner-svg"><title>Test</title></svg>',
                    title: "Test chart",
                    attachment_id: "att-1",
                }}
            />,
        );
        const fig = screen.getByTestId("chart-inline");
        expect(fig).toHaveAttribute("data-attachment-id", "att-1");
        // The SVG was dropped into the DOM via dangerouslySetInnerHTML.
        // We assert on the `<title>` content that came from the SVG.
        expect(fig.innerHTML).toContain("<title>Test</title>");
    });

    it("renders the title and a download link when given an attachment_id", async () => {
        const Chart = getChatComponent("chart")!;
        render(
            <AuthContext.Provider value={fakeAuth()}>
                <Chart
                    data={{
                        chat_component_kind: "chart",
                        svg: "<svg/>",
                        title: "Forecast",
                        attachment_id: "att-2",
                    }}
                />
            </AuthContext.Provider>,
        );
        await waitFor(() => {
            const link = screen.getByTestId(
                "chart-inline-download",
            ) as HTMLAnchorElement;
            expect(link.getAttribute("href") ?? "").toContain(
                "/api/attachments/att-2",
            );
            expect(link.getAttribute("href") ?? "").toContain("sig=");
            // Audit bar: no raw JWT in the URL.
            expect(link.getAttribute("href") ?? "").not.toMatch(/access_token=/);
        });
    });

    it("renders the fallback when the SVG is missing", () => {
        const Chart = getChatComponent("chart")!;
        render(
            <Chart data={{ chat_component_kind: "chart", attachment_id: "att-3" }} />,
        );
        expect(screen.getByText(/chart not available/i)).toBeInTheDocument();
    });
});

describe("WeatherCurrent component", () => {
    it("renders place + temperature + condition", () => {
        const C = getChatComponent("weather_current")!;
        render(
            <C
                data={{
                    chat_component_kind: "weather_current",
                    place_name: "Reykjavík",
                    temperature_unit: "celsius",
                    current: {
                        temperature_2m: 7,
                        apparent_temperature: 4,
                        weather_code: 61,
                        wind_speed_10m: 18,
                        relative_humidity_2m: 82,
                        precipitation: 0.5,
                    },
                }}
            />,
        );
        const card = screen.getByTestId("weather-current");
        expect(card.textContent).toContain("Reykjavík");
        expect(card.textContent).toContain("Light rain");
        expect(card.textContent).toContain("7°C");
        expect(card.textContent).toContain("feels 4°C");
    });

    it("degrades gracefully on missing fields", () => {
        const C = getChatComponent("weather_current")!;
        render(
            <C
                data={{
                    chat_component_kind: "weather_current",
                    current: {},
                }}
            />,
        );
        const card = screen.getByTestId("weather-current");
        // "—" is the missing-value sentinel
        expect(card.textContent).toContain("—");
    });
});

describe("WeatherDaily component", () => {
    it("renders one row per day with min/max + precip", () => {
        const C = getChatComponent("weather_daily")!;
        render(
            <C
                data={{
                    chat_component_kind: "weather_daily",
                    place_name: "Reykjavík",
                    temperature_unit: "celsius",
                    precipitation_unit: "mm",
                    daily: {
                        time: ["2026-05-12", "2026-05-13"],
                        temperature_2m_max: [11, 9],
                        temperature_2m_min: [4, 3],
                        precipitation_sum: [2.1, 0],
                        precipitation_probability_max: [80, 10],
                        weather_code: [61, 1],
                    },
                }}
            />,
        );
        const table = screen.getByTestId("weather-daily");
        expect(table.textContent).toContain("Reykjavík");
        expect(table.textContent).toContain("11°C");
        expect(table.textContent).toContain("80%");
    });
});
