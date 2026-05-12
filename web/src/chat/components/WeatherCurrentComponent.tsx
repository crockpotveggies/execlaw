// Compact "current conditions" card for tool_result payloads with
// `chat_component_kind: "weather_current"`.
//
// The open-meteo plugin's forecast tool emits a payload that mirrors
// the Open-Meteo response shape plus a few derived fields. We
// deliberately don't reshape it server-side — the plugin already
// trims to the fields the agent will care about, and the SPA reads
// only the fields it needs.

import {
    registerChatComponent,
    type ChatComponentProps,
} from "../chatComponentRegistry";

interface CurrentBlock {
    time?: string;
    temperature_2m?: number;
    apparent_temperature?: number;
    relative_humidity_2m?: number;
    weather_code?: number;
    wind_speed_10m?: number;
    wind_direction_10m?: number;
    precipitation?: number;
}

// WMO weather-code → short label. Authoritative table from
// https://open-meteo.com/en/docs (the same mapping selfhosted-claw
// used). Unknown codes degrade to "—".
const WMO: Record<number, string> = {
    0: "Clear",
    1: "Mainly clear",
    2: "Partly cloudy",
    3: "Overcast",
    45: "Fog",
    48: "Depositing rime fog",
    51: "Light drizzle",
    53: "Drizzle",
    55: "Heavy drizzle",
    61: "Light rain",
    63: "Rain",
    65: "Heavy rain",
    71: "Light snow",
    73: "Snow",
    75: "Heavy snow",
    77: "Snow grains",
    80: "Rain showers",
    81: "Rain showers",
    82: "Violent rain showers",
    85: "Snow showers",
    86: "Heavy snow showers",
    95: "Thunderstorm",
    96: "Thunderstorm with hail",
    99: "Severe thunderstorm",
};

function wmoLabel(code: number | undefined): string {
    if (code === undefined || code === null) return "—";
    return WMO[code] ?? `code ${code}`;
}

function fmt(value: number | undefined, suffix: string): string {
    if (value === null || value === undefined || Number.isNaN(value)) return "—";
    return `${value}${suffix}`;
}

function WeatherCurrent({ data }: ChatComponentProps) {
    const current = (data.current as CurrentBlock | undefined) ?? {};
    const placeName =
        typeof data.place_name === "string" ? (data.place_name as string) : null;
    const tempUnit =
        typeof data.temperature_unit === "string"
            ? (data.temperature_unit as string) === "fahrenheit"
                ? "°F"
                : "°C"
            : "°C";
    const windUnit =
        typeof data.wind_speed_unit === "string"
            ? (data.wind_speed_unit as string)
            : "km/h";
    const precipUnit =
        typeof data.precipitation_unit === "string"
            ? (data.precipitation_unit as string) === "inch"
                ? "in"
                : "mm"
            : "mm";

    return (
        <div
            className="execlaw-weather-current"
            data-testid="weather-current"
        >
            <div className="execlaw-weather-current__head">
                <span className="execlaw-weather-current__place">
                    {placeName ?? "Current conditions"}
                </span>
                <span className="execlaw-weather-current__condition execlaw-muted small">
                    {wmoLabel(current.weather_code)}
                </span>
            </div>
            <div className="execlaw-weather-current__grid">
                <Stat
                    label="Temp"
                    value={fmt(current.temperature_2m, tempUnit)}
                    sub={
                        current.apparent_temperature !== undefined
                            ? `feels ${fmt(current.apparent_temperature, tempUnit)}`
                            : undefined
                    }
                />
                <Stat label="Wind" value={fmt(current.wind_speed_10m, ` ${windUnit}`)} />
                <Stat label="Humidity" value={fmt(current.relative_humidity_2m, "%")} />
                <Stat
                    label="Precip"
                    value={fmt(current.precipitation, ` ${precipUnit}`)}
                />
            </div>
        </div>
    );
}

function Stat({
    label,
    value,
    sub,
}: {
    label: string;
    value: string;
    sub?: string;
}) {
    return (
        <div className="execlaw-weather-current__stat">
            <div className="execlaw-weather-current__stat-label execlaw-muted small">
                {label}
            </div>
            <div className="execlaw-weather-current__stat-value">{value}</div>
            {sub && (
                <div className="execlaw-weather-current__stat-sub execlaw-muted small">
                    {sub}
                </div>
            )}
        </div>
    );
}

registerChatComponent("weather_current", WeatherCurrent);

export default WeatherCurrent;
