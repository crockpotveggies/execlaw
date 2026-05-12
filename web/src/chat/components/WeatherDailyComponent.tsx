// Multi-day daily-forecast table for tool_result payloads with
// `chat_component_kind: "weather_daily"`.
//
// Reads the Open-Meteo `daily` block (column-oriented arrays —
// `time[], temperature_2m_max[], temperature_2m_min[],
// precipitation_sum[], weather_code[], precipitation_probability_max[]`)
// and zips them into a row-per-day table. Missing columns fall back
// to "—" so partial responses (the plugin author dropped a field
// from the `daily=...` query) still render usable output.

import {
    registerChatComponent,
    type ChatComponentProps,
} from "../chatComponentRegistry";

interface DailyBlock {
    time?: string[];
    temperature_2m_max?: number[];
    temperature_2m_min?: number[];
    precipitation_sum?: number[];
    precipitation_probability_max?: number[];
    weather_code?: number[];
}

const WMO_ICON: Record<number, string> = {
    0: "bi-sun",
    1: "bi-sun",
    2: "bi-cloud-sun",
    3: "bi-clouds",
    45: "bi-cloud-fog2",
    48: "bi-cloud-fog2",
    51: "bi-cloud-drizzle",
    53: "bi-cloud-drizzle",
    55: "bi-cloud-drizzle",
    61: "bi-cloud-rain",
    63: "bi-cloud-rain",
    65: "bi-cloud-rain-heavy",
    71: "bi-cloud-snow",
    73: "bi-cloud-snow",
    75: "bi-cloud-snow",
    80: "bi-cloud-rain",
    81: "bi-cloud-rain",
    82: "bi-cloud-rain-heavy",
    85: "bi-cloud-snow",
    86: "bi-cloud-snow",
    95: "bi-cloud-lightning-rain",
    96: "bi-cloud-lightning-rain",
    99: "bi-cloud-lightning-rain",
};

function dayLabel(iso: string | undefined): string {
    if (!iso) return "—";
    // Expect either YYYY-MM-DD or full ISO; both parse cleanly.
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleDateString(undefined, {
        weekday: "short",
        month: "short",
        day: "numeric",
    });
}

function fmt(n: number | undefined, suffix: string): string {
    if (n === null || n === undefined || Number.isNaN(n)) return "—";
    return `${n}${suffix}`;
}

function WeatherDaily({ data }: ChatComponentProps) {
    const daily = (data.daily as DailyBlock | undefined) ?? {};
    const placeName =
        typeof data.place_name === "string" ? (data.place_name as string) : null;
    const times = Array.isArray(daily.time) ? daily.time : [];
    const tempUnit =
        typeof data.temperature_unit === "string"
            ? (data.temperature_unit as string) === "fahrenheit"
                ? "°F"
                : "°C"
            : "°C";
    const precipUnit =
        typeof data.precipitation_unit === "string"
            ? (data.precipitation_unit as string) === "inch"
                ? "in"
                : "mm"
            : "mm";

    return (
        <div className="execlaw-weather-daily" data-testid="weather-daily">
            {placeName && (
                <div className="execlaw-weather-daily__head">
                    <span className="execlaw-weather-daily__place">
                        {placeName}
                    </span>
                    <span className="execlaw-weather-daily__sub execlaw-muted small">
                        {times.length}-day forecast
                    </span>
                </div>
            )}
            <table className="execlaw-weather-daily__table">
                <thead>
                    <tr>
                        <th>Day</th>
                        <th />
                        <th>High</th>
                        <th>Low</th>
                        <th>Precip</th>
                    </tr>
                </thead>
                <tbody>
                    {times.map((iso, i) => {
                        const code = daily.weather_code?.[i];
                        const icon =
                            code !== undefined ? WMO_ICON[code] ?? "bi-circle" : "bi-circle";
                        return (
                            <tr key={iso ?? i}>
                                <td>{dayLabel(iso)}</td>
                                <td>
                                    <i className={`bi ${icon}`} aria-hidden />
                                </td>
                                <td>
                                    {fmt(daily.temperature_2m_max?.[i], tempUnit)}
                                </td>
                                <td>
                                    {fmt(daily.temperature_2m_min?.[i], tempUnit)}
                                </td>
                                <td>
                                    {fmt(daily.precipitation_sum?.[i], ` ${precipUnit}`)}
                                    {daily.precipitation_probability_max?.[i] !==
                                        undefined && (
                                        <span className="execlaw-muted small">
                                            {" "}
                                            ({daily.precipitation_probability_max[i]}%)
                                        </span>
                                    )}
                                </td>
                            </tr>
                        );
                    })}
                </tbody>
            </table>
        </div>
    );
}

registerChatComponent("weather_daily", WeatherDaily);

export default WeatherDaily;
