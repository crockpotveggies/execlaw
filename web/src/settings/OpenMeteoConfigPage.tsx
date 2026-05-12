// Settings → Plugins → Open-Meteo.
//
// Open-Meteo is keyless + free, so the entire form is about default
// preferences:
//   * Default location — operator picks a place that the agent will
//     use when no explicit coordinate is supplied. Lat/lon are
//     resolved server-side via the open_meteo.geocode tool when the
//     operator types a place name and clicks "Resolve".
//   * Unit triplet — temperature (°C / °F), wind (kmh / mph / ms / kn),
//     precipitation (mm / inch).
//   * Default chart dimensions — used by open_meteo.render_chart when
//     the agent doesn't pass explicit width/height.
//
// The Test button hits POST /api/admin/plugins/open-meteo/test which
// issues a 1-call forecast lookup at the saved coordinates — useful
// as a connectivity check (Open-Meteo down? DNS hijack? rate-limited?).

import { useCallback, useEffect, useState, type JSX } from "react";
import { Alert, Button, Card, Form, Spinner } from "react-bootstrap";
import {
    getOpenMeteoConfig,
    setOpenMeteoConfig,
    testOpenMeteoForecast,
    type OpenMeteoConfigResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

const TIMEZONE_PRESETS = [
    "auto",
    "UTC",
    "America/New_York",
    "America/Los_Angeles",
    "Europe/London",
    "Europe/Berlin",
    "Asia/Tokyo",
];

export function OpenMeteoConfigPage(_props: PluginConfigProps): JSX.Element {
    const { getAccessToken } = useAuth();
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

    const reload = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const c = await getOpenMeteoConfig(getAccessToken);
            applyConfig(c);
        } catch (e) {
            setError(e instanceof Error ? e.message : "couldn't load config");
        } finally {
            setLoading(false);
        }
    }, [getAccessToken]);

    const applyConfig = useCallback((c: OpenMeteoConfigResponse) => {
        setPlaceName(c.place_name ?? "");
        setLat(c.default_latitude !== null && c.default_latitude !== undefined
            ? String(c.default_latitude)
            : "");
        setLon(c.default_longitude !== null && c.default_longitude !== undefined
            ? String(c.default_longitude)
            : "");
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
            if (latNum !== null && (!Number.isFinite(latNum) || latNum < -90 || latNum > 90)) {
                setError("Latitude must be a number between -90 and 90.");
                setBusy(false);
                return;
            }
            if (lonNum !== null && (!Number.isFinite(lonNum) || lonNum < -180 || lonNum > 180)) {
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
            await setOpenMeteoConfig(
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
                getAccessToken,
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
        getAccessToken,
        reload,
    ]);

    const onTest = useCallback(async () => {
        setBusy(true);
        setError(null);
        setTestStatus({ kind: "idle" });
        try {
            const r = await testOpenMeteoForecast(getAccessToken);
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
    }, [getAccessToken]);

    if (loading) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <Spinner animation="border" size="sm" className="me-2" />
                Loading…
            </div>
        );
    }

    return (
        <div data-testid="open-meteo-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Default location</h5>
                    <p className="execlaw-muted small mb-3">
                        When the operator asks the agent about weather without
                        specifying a place, the agent uses this default. Open-Meteo
                        accepts raw latitude/longitude only; resolve a place name
                        via the agent (it'll call the <code>geocode</code> tool).
                    </p>

                    {savedNotice && (
                        <Alert variant="success" data-testid="open-meteo-saved">
                            {savedNotice}
                        </Alert>
                    )}

                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Place name (label)
                        </Form.Label>
                        <Form.Control
                            type="text"
                            placeholder="Reykjavík"
                            value={placeName}
                            onChange={(e) => setPlaceName(e.target.value)}
                            data-testid="open-meteo-place-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Echoed back in the chat card so the operator sees
                            the place by name. Cosmetic — the agent uses
                            lat/lon, not this string.
                        </Form.Text>
                    </Form.Group>

                    <div className="row g-2 mb-2">
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Latitude
                            </Form.Label>
                            <Form.Control
                                type="number"
                                step="any"
                                placeholder="64.146"
                                value={lat}
                                onChange={(e) => setLat(e.target.value)}
                                data-testid="open-meteo-lat-input"
                            />
                        </div>
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Longitude
                            </Form.Label>
                            <Form.Control
                                type="number"
                                step="any"
                                placeholder="-21.940"
                                value={lon}
                                onChange={(e) => setLon(e.target.value)}
                                data-testid="open-meteo-lon-input"
                            />
                        </div>
                    </div>

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Timezone
                        </Form.Label>
                        <Form.Select
                            value={timezone}
                            onChange={(e) => setTimezone(e.target.value)}
                            data-testid="open-meteo-tz-input"
                        >
                            {TIMEZONE_PRESETS.map((tz) => (
                                <option key={tz} value={tz}>
                                    {tz}
                                </option>
                            ))}
                            {!TIMEZONE_PRESETS.includes(timezone) && (
                                <option value={timezone}>{timezone}</option>
                            )}
                        </Form.Select>
                        <Form.Text className="execlaw-muted">
                            <code>auto</code> resolves to the coordinate's local
                            zone. Pick an IANA tz to pin a different one.
                        </Form.Text>
                    </Form.Group>
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Units</h5>
                    <div className="row g-2 mb-2">
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Temperature
                            </Form.Label>
                            <Form.Select
                                value={tempUnit}
                                onChange={(e) =>
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
                            </Form.Select>
                        </div>
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Wind speed
                            </Form.Label>
                            <Form.Select
                                value={windUnit}
                                onChange={(e) =>
                                    setWindUnit(e.target.value as typeof windUnit)
                                }
                                data-testid="open-meteo-wind-unit-input"
                            >
                                <option value="kmh">km/h</option>
                                <option value="mph">mph</option>
                                <option value="ms">m/s</option>
                                <option value="kn">knots</option>
                            </Form.Select>
                        </div>
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Precipitation
                            </Form.Label>
                            <Form.Select
                                value={precipUnit}
                                onChange={(e) =>
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
                            </Form.Select>
                        </div>
                    </div>
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Charts</h5>
                    <div className="row g-2">
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Default width (px)
                            </Form.Label>
                            <Form.Control
                                type="number"
                                min={240}
                                max={2400}
                                value={chartWidth}
                                onChange={(e) => setChartWidth(e.target.value)}
                                data-testid="open-meteo-chart-width-input"
                            />
                        </div>
                        <div className="col">
                            <Form.Label className="execlaw-muted small mb-1">
                                Default height (px)
                            </Form.Label>
                            <Form.Control
                                type="number"
                                min={240}
                                max={2400}
                                value={chartHeight}
                                onChange={(e) => setChartHeight(e.target.value)}
                                data-testid="open-meteo-chart-height-input"
                            />
                        </div>
                    </div>
                    <Form.Text className="execlaw-muted">
                        Used by the <code>render_chart</code> tool when the
                        agent doesn't specify dimensions. Values clamp to
                        240..2400 server-side.
                    </Form.Text>
                </Card.Body>
            </Card>

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
                <Alert variant="success" data-testid="open-meteo-test-ok">
                    {testStatus.message}
                </Alert>
            )}
            {testStatus.kind === "err" && (
                <Alert variant="danger" data-testid="open-meteo-test-err">
                    {testStatus.message}
                </Alert>
            )}
        </div>
    );
}
