// Open-Meteo plugin self-contained config panel.
//
// Migrated from `web/src/settings/OpenMeteoConfigPage.tsx` (2026-05-14).
// Build: node scripts/build-plugin-ui.mjs open-meteo

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface OpenMeteoConfigResponse {
    place_name?: string | null;
    default_latitude?: number | null;
    default_longitude?: number | null;
    default_timezone?: string;
    temperature_unit?: string;
    wind_speed_unit?: string;
    precipitation_unit?: string;
    default_chart_width?: number;
    default_chart_height?: number;
}

interface OpenMeteoTestResponse {
    ok?: boolean;
    latitude?: number;
    longitude?: number;
    current?: Record<string, unknown>;
    error?: string;
}

const TIMEZONE_PRESETS = [
    "auto",
    "UTC",
    "America/New_York",
    "America/Los_Angeles",
    "Europe/London",
    "Europe/Berlin",
    "Asia/Tokyo",
];

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge } = props;
    const { ErrorBanner, Button } = bridge.components;

    const [loading, setLoading] = useState(true);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [savedNotice, setSavedNotice] = useState<string | null>(null);

    const [placeName, setPlaceName] = useState("");
    const [lat, setLat] = useState("");
    const [lon, setLon] = useState("");
    const [timezone, setTimezone] = useState("auto");
    const [tempUnit, setTempUnit] = useState<"celsius" | "fahrenheit">(
        "celsius",
    );
    const [windUnit, setWindUnit] = useState<"kmh" | "ms" | "mph" | "kn">(
        "kmh",
    );
    const [precipUnit, setPrecipUnit] = useState<"mm" | "inch">("mm");
    const [chartWidth, setChartWidth] = useState("720");
    const [chartHeight, setChartHeight] = useState("400");

    const [testStatus, setTestStatus] = useState<
        | { kind: "idle" }
        | { kind: "ok"; message: string }
        | { kind: "err"; message: string }
    >({ kind: "idle" });

    const applyConfig = useCallback((c: OpenMeteoConfigResponse) => {
        setPlaceName(c.place_name ?? "");
        setLat(
            c.default_latitude !== null && c.default_latitude !== undefined
                ? String(c.default_latitude)
                : "",
        );
        setLon(
            c.default_longitude !== null && c.default_longitude !== undefined
                ? String(c.default_longitude)
                : "",
        );
        setTimezone(c.default_timezone ?? "auto");
        if (c.temperature_unit === "fahrenheit") setTempUnit("fahrenheit");
        else setTempUnit("celsius");
        if (
            c.wind_speed_unit === "ms" ||
            c.wind_speed_unit === "mph" ||
            c.wind_speed_unit === "kn"
        )
            setWindUnit(c.wind_speed_unit);
        else setWindUnit("kmh");
        setPrecipUnit(c.precipitation_unit === "inch" ? "inch" : "mm");
        setChartWidth(String(c.default_chart_width ?? 720));
        setChartHeight(String(c.default_chart_height ?? 400));
    }, []);

    const reload = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const c = await bridge.fetchJson<OpenMeteoConfigResponse>(
                "GET",
                "/api/admin/plugins/open-meteo/config",
            );
            applyConfig(c);
        } catch (e) {
            setError(e instanceof Error ? e.message : "couldn't load config");
        } finally {
            setLoading(false);
        }
    }, [bridge, applyConfig]);

    useEffect(() => {
        void reload();
    }, [reload]);

    const onSave = useCallback(async () => {
        setBusy(true);
        setError(null);
        setSavedNotice(null);
        setTestStatus({ kind: "idle" });
        try {
            const latNum = lat.trim() === "" ? null : Number(lat);
            const lonNum = lon.trim() === "" ? null : Number(lon);
            if (
                latNum !== null &&
                (!Number.isFinite(latNum) || latNum < -90 || latNum > 90)
            ) {
                setError("Latitude must be a number between -90 and 90.");
                setBusy(false);
                return;
            }
            if (
                lonNum !== null &&
                (!Number.isFinite(lonNum) || lonNum < -180 || lonNum > 180)
            ) {
                setError("Longitude must be a number between -180 and 180.");
                setBusy(false);
                return;
            }
            const widthNum = Number(chartWidth);
            const heightNum = Number(chartHeight);
            if (
                !Number.isFinite(widthNum) ||
                widthNum < 240 ||
                widthNum > 2400 ||
                !Number.isFinite(heightNum) ||
                heightNum < 240 ||
                heightNum > 2400
            ) {
                setError(
                    "Chart width/height must be integers between 240 and 2400.",
                );
                setBusy(false);
                return;
            }
            await bridge.fetchJson<unknown>(
                "POST",
                "/api/admin/plugins/open-meteo/config",
                {
                    place_name: placeName.trim() || null,
                    default_latitude: latNum,
                    default_longitude: lonNum,
                    default_timezone: timezone,
                    temperature_unit: tempUnit,
                    wind_speed_unit: windUnit,
                    precipitation_unit: precipUnit,
                    default_chart_width: widthNum,
                    default_chart_height: heightNum,
                },
            );
            setSavedNotice("Saved.");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [
        placeName,
        lat,
        lon,
        timezone,
        tempUnit,
        windUnit,
        precipUnit,
        chartWidth,
        chartHeight,
        bridge,
        reload,
    ]);

    const onTest = useCallback(async () => {
        setBusy(true);
        setError(null);
        setTestStatus({ kind: "idle" });
        try {
            const r = await bridge.fetchJson<OpenMeteoTestResponse>(
                "POST",
                "/api/admin/plugins/open-meteo/test",
            );
            if (r.ok === false || r.error) {
                setTestStatus({
                    kind: "err",
                    message: r.error ?? "Open-Meteo rejected the request.",
                });
            } else {
                const temp =
                    r.current && typeof r.current["temperature_2m"] === "number"
                        ? `${(r.current as Record<string, number>)["temperature_2m"]}°`
                        : "?";
                setTestStatus({
                    kind: "ok",
                    message: `Open-Meteo replied — current temperature ${temp} at ${r.latitude}, ${r.longitude}.`,
                });
            }
        } catch (e) {
            setTestStatus({
                kind: "err",
                message: e instanceof Error ? e.message : String(e),
            });
        } finally {
            setBusy(false);
        }
    }, [bridge]);

    if (loading) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <span
                    className="spinner-border spinner-border-sm me-2"
                    role="status"
                    aria-hidden
                />
                Loading…
            </div>
        );
    }

    const tzOptions = TIMEZONE_PRESETS.includes(timezone)
        ? TIMEZONE_PRESETS
        : [...TIMEZONE_PRESETS, timezone];

    return (
        <div data-testid="open-meteo-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Default location</h5>
                    <p className="execlaw-muted small mb-3">
                        When the operator asks the agent about weather without
                        specifying a place, the agent uses this default. Open-Meteo
                        accepts raw latitude/longitude only; resolve a place name
                        via the agent (it&apos;ll call the <code>geocode</code> tool).
                    </p>

                    {savedNotice && (
                        <div className="alert alert-success" data-testid="open-meteo-saved">
                            {savedNotice}
                        </div>
                    )}

                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            Place name (label)
                        </label>
                        <input
                            type="text"
                            className="form-control"
                            placeholder="Reykjavík"
                            value={placeName}
                            onChange={(e: { target: { value: string } }) =>
                                setPlaceName(e.target.value)
                            }
                            data-testid="open-meteo-place-input"
                        />
                        <div className="form-text execlaw-muted">
                            Echoed back in the chat card so the operator sees
                            the place by name. Cosmetic — the agent uses
                            lat/lon, not this string.
                        </div>
                    </div>

                    <div className="row g-2 mb-2">
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Latitude
                            </label>
                            <input
                                type="number"
                                step="any"
                                className="form-control"
                                placeholder="64.146"
                                value={lat}
                                onChange={(e: { target: { value: string } }) =>
                                    setLat(e.target.value)
                                }
                                data-testid="open-meteo-lat-input"
                            />
                        </div>
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Longitude
                            </label>
                            <input
                                type="number"
                                step="any"
                                className="form-control"
                                placeholder="-21.940"
                                value={lon}
                                onChange={(e: { target: { value: string } }) =>
                                    setLon(e.target.value)
                                }
                                data-testid="open-meteo-lon-input"
                            />
                        </div>
                    </div>

                    <div className="mb-3">
                        <label className="form-label execlaw-muted small mb-1">
                            Timezone
                        </label>
                        <select
                            className="form-select"
                            value={timezone}
                            onChange={(e: { target: { value: string } }) =>
                                setTimezone(e.target.value)
                            }
                            data-testid="open-meteo-tz-input"
                        >
                            {tzOptions.map((tz: string) => (
                                <option key={tz} value={tz}>
                                    {tz}
                                </option>
                            ))}
                        </select>
                        <div className="form-text execlaw-muted">
                            <code>auto</code> resolves to the coordinate&apos;s local
                            zone. Pick an IANA tz to pin a different one.
                        </div>
                    </div>
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Units</h5>
                    <div className="row g-2 mb-2">
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Temperature
                            </label>
                            <select
                                className="form-select"
                                value={tempUnit}
                                onChange={(e: { target: { value: string } }) =>
                                    setTempUnit(
                                        e.target.value === "fahrenheit"
                                            ? "fahrenheit"
                                            : "celsius",
                                    )
                                }
                                data-testid="open-meteo-temp-unit-input"
                            >
                                <option value="celsius">°C</option>
                                <option value="fahrenheit">°F</option>
                            </select>
                        </div>
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Wind speed
                            </label>
                            <select
                                className="form-select"
                                value={windUnit}
                                onChange={(e: { target: { value: string } }) =>
                                    setWindUnit(
                                        e.target.value as
                                            | "kmh"
                                            | "ms"
                                            | "mph"
                                            | "kn",
                                    )
                                }
                                data-testid="open-meteo-wind-unit-input"
                            >
                                <option value="kmh">km/h</option>
                                <option value="mph">mph</option>
                                <option value="ms">m/s</option>
                                <option value="kn">knots</option>
                            </select>
                        </div>
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Precipitation
                            </label>
                            <select
                                className="form-select"
                                value={precipUnit}
                                onChange={(e: { target: { value: string } }) =>
                                    setPrecipUnit(
                                        e.target.value === "inch"
                                            ? "inch"
                                            : "mm",
                                    )
                                }
                                data-testid="open-meteo-precip-unit-input"
                            >
                                <option value="mm">mm</option>
                                <option value="inch">inch</option>
                            </select>
                        </div>
                    </div>
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Charts</h5>
                    <div className="row g-2">
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Default width (px)
                            </label>
                            <input
                                type="number"
                                min={240}
                                max={2400}
                                className="form-control"
                                value={chartWidth}
                                onChange={(e: { target: { value: string } }) =>
                                    setChartWidth(e.target.value)
                                }
                                data-testid="open-meteo-chart-width-input"
                            />
                        </div>
                        <div className="col">
                            <label className="form-label execlaw-muted small mb-1">
                                Default height (px)
                            </label>
                            <input
                                type="number"
                                min={240}
                                max={2400}
                                className="form-control"
                                value={chartHeight}
                                onChange={(e: { target: { value: string } }) =>
                                    setChartHeight(e.target.value)
                                }
                                data-testid="open-meteo-chart-height-input"
                            />
                        </div>
                    </div>
                    <div className="form-text execlaw-muted">
                        Used by the <code>render_chart</code> tool when the
                        agent doesn&apos;t specify dimensions. Values clamp to
                        240..2400 server-side.
                    </div>
                </div>
            </div>

            <div className="d-flex gap-2 mb-3">
                <Button
                    variant="primary"
                    size="sm"
                    onClick={() => void onSave()}
                    disabled={busy}
                    data-testid="open-meteo-save"
                >
                    Save
                </Button>
                <Button
                    variant="outline-secondary"
                    size="sm"
                    onClick={() => void onTest()}
                    disabled={busy}
                    data-testid="open-meteo-test"
                >
                    Test connectivity
                </Button>
            </div>

            {testStatus.kind === "ok" && (
                <div className="alert alert-success" data-testid="open-meteo-test-ok">
                    {testStatus.message}
                </div>
            )}
            {testStatus.kind === "err" && (
                <div className="alert alert-danger" data-testid="open-meteo-test-err">
                    {testStatus.message}
                </div>
            )}
        </div>
    );
};

export default Panel;
