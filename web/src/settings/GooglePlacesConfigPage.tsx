// Settings → Plugins → Google Places.
//
// Operator pastes a Google Maps API key (with Places API (New)
// enabled in their Google Cloud project) and picks a cost tier.
// Save validates by issuing a 1-result `coffee` search before
// persisting; the masked-tail of the key + last validation
// timestamp surface in the form so the operator can confirm
// "yes, the right key is stored."
//
// Three affordances:
//   * Save form — write the API key + cost tier + default
//     max-results to the plugin's vault. Validates the key
//     before persisting so a bad paste fails immediately.
//   * Test button — fire a sample text search and surface the
//     count + first result name as a sanity check.
//   * Status card — surfaces last-validated-at + any cached
//     validation error so the operator can spot a key that
//     stopped working.

import { useCallback, useEffect, useState, type JSX } from "react";
import { Alert, Badge, Button, Card, Form, Spinner } from "react-bootstrap";
import {
    getGooglePlacesConfig,
    getGooglePlacesStatus,
    setGooglePlacesConfig,
    testGooglePlaces,
    type GooglePlacesConfigResponse,
    type GooglePlacesStatusResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

export function GooglePlacesConfigPage(_props: PluginConfigProps): JSX.Element {
    const { getAccessToken } = useAuth();
    const [config, setConfig] = useState<GooglePlacesConfigResponse | null>(null);
    const [status, setStatus] = useState<GooglePlacesStatusResponse | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [apiKey, setApiKey] = useState("");
    const [costTier, setCostTier] = useState<"pro" | "essentials">("pro");
    const [defaultMax, setDefaultMax] = useState("");
    const [savedNotice, setSavedNotice] = useState<string | null>(null);
    const [testQuery, setTestQuery] = useState("coffee near me");
    const [testStatus, setTestStatus] = useState<
        | { kind: "idle" }
        | { kind: "ok"; message: string }
        | { kind: "err"; message: string }
    >({ kind: "idle" });

    const reload = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const [c, s] = await Promise.all([
                getGooglePlacesConfig(getAccessToken),
                getGooglePlacesStatus(getAccessToken),
            ]);
            setConfig(c);
            setStatus(s);
            setCostTier(c.cost_tier === "essentials" ? "essentials" : "pro");
            setDefaultMax(
                c.default_max_results > 0 ? String(c.default_max_results) : "",
            );
        } catch (e) {
            setError(e instanceof Error ? e.message : "couldn't load config");
        } finally {
            setLoading(false);
        }
    }, [getAccessToken]);

    useEffect(() => {
        void reload();
    }, [reload]);

    const onSave = useCallback(async () => {
        setBusy(true);
        setError(null);
        setSavedNotice(null);
        setTestStatus({ kind: "idle" });
        try {
            const parsedMax = defaultMax.trim() === "" ? null : Number(defaultMax);
            if (parsedMax !== null && (!Number.isFinite(parsedMax) || parsedMax < 1 || parsedMax > 20)) {
                setError("Default max results must be between 1 and 20.");
                setBusy(false);
                return;
            }
            await setGooglePlacesConfig(apiKey, costTier, parsedMax, getAccessToken);
            setSavedNotice("Saved. The API key was validated against Google Places.");
            setApiKey("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [apiKey, costTier, defaultMax, getAccessToken, reload]);

    const onTest = useCallback(async () => {
        const q = testQuery.trim();
        if (q === "") {
            setTestStatus({ kind: "err", message: "Enter a query first." });
            return;
        }
        setBusy(true);
        setTestStatus({ kind: "idle" });
        setError(null);
        try {
            const r = await testGooglePlaces(q, getAccessToken);
            if (r.ok === false) {
                setTestStatus({
                    kind: "err",
                    message: r.error ?? "Google Places rejected the request.",
                });
            } else {
                const count = r.returned_count ?? 0;
                const first = r.first_result_name ?? "";
                setTestStatus({
                    kind: "ok",
                    message:
                        count > 0
                            ? `${count} result(s). First: ${first || "(unnamed)"}.`
                            : "Request succeeded but returned 0 results.",
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
    }, [getAccessToken, testQuery]);

    if (loading) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <Spinner animation="border" size="sm" className="me-2" />
                Loading…
            </div>
        );
    }

    const apiKeySet = config?.api_key_set ?? false;
    const stateLabel = status?.state ?? "unconfigured";
    const stateBadge =
        stateLabel === "online"
            ? "bg-success"
            : stateLabel === "configured"
              ? "bg-warning text-dark"
              : stateLabel === "degraded"
                ? "bg-danger"
                : "bg-warning text-dark";

    return (
        <div data-testid="google-places-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <Card className="mb-3">
                <Card.Body>
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Google Places API</h5>
                        <Badge
                            bg=""
                            className={stateBadge}
                            data-testid="google-places-status-badge"
                        >
                            {stateLabel}
                        </Badge>
                    </div>
                    <p className="execlaw-muted small mb-3">
                        Enable <strong>Places API (New)</strong> in your Google
                        Cloud project at{" "}
                        <a
                            href="https://console.cloud.google.com/apis/library/places.googleapis.com"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            console.cloud.google.com
                        </a>
                        , then create an API key under{" "}
                        <a
                            href="https://console.cloud.google.com/apis/credentials"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            APIs &amp; Services → Credentials
                        </a>
                        . Restrict the key to "Places API (New)" so a leak
                        can't run up your Maps Geocoding bill.
                    </p>

                    {savedNotice && (
                        <Alert variant="success" data-testid="google-places-saved">
                            {savedNotice}
                        </Alert>
                    )}
                    {status?.validation_error && (
                        <Alert variant="danger">
                            Last validation error:{" "}
                            <code>{status.validation_error}</code>
                        </Alert>
                    )}

                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            API key
                            {apiKeySet && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config?.api_key_masked}</code>)
                                </span>
                            )}
                        </Form.Label>
                        <Form.Control
                            type="password"
                            placeholder="AIza..."
                            value={apiKey}
                            onChange={(e) => setApiKey(e.target.value)}
                            data-testid="google-places-api-key-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Stored locally; never leaves the host. Save validates
                            the key by issuing a 1-result test search before
                            persisting.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Cost tier
                        </Form.Label>
                        <Form.Select
                            value={costTier}
                            onChange={(e) =>
                                setCostTier(
                                    e.target.value === "essentials"
                                        ? "essentials"
                                        : "pro",
                                )
                            }
                            data-testid="google-places-cost-tier-input"
                        >
                            <option value="pro">
                                Pro — adds hours, phone, website (more $/call)
                            </option>
                            <option value="essentials">
                                Essentials — basic info only (cheaper)
                            </option>
                        </Form.Select>
                        <Form.Text className="execlaw-muted">
                            Google Places (New) prices per field-class. Pro
                            covers most agent uses; Essentials is fine when
                            the agent only needs name + address + rating.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Default max results{" "}
                            <span className="execlaw-muted">(1-20)</span>
                        </Form.Label>
                        <Form.Control
                            type="number"
                            min={1}
                            max={20}
                            placeholder="5"
                            value={defaultMax}
                            onChange={(e) => setDefaultMax(e.target.value)}
                            data-testid="google-places-default-max-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Used when an agent's <code>search</code> call
                            doesn't specify <code>max_results</code>. Empty
                            falls back to 5.
                        </Form.Text>
                    </Form.Group>

                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={busy}
                            data-testid="google-places-save"
                        >
                            Save
                        </Button>
                    </div>
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Test search</h5>
                    <p className="execlaw-muted small mb-2">
                        Issue a sample <code>searchText</code> call. Returns
                        the result count + first place name so you can confirm
                        the key + tier are wired correctly. Costs 1 call
                        against your Google quota.
                    </p>
                    <Form.Group className="mb-2">
                        <Form.Control
                            type="text"
                            placeholder="coffee near me"
                            value={testQuery}
                            onChange={(e) => setTestQuery(e.target.value)}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                    e.preventDefault();
                                    void onTest();
                                }
                            }}
                            data-testid="google-places-test-query-input"
                        />
                    </Form.Group>
                    <Button
                        size="sm"
                        variant="outline-secondary"
                        onClick={() => void onTest()}
                        disabled={busy || !apiKeySet}
                        data-testid="google-places-test"
                    >
                        Send test search
                    </Button>
                    {testStatus.kind === "ok" && (
                        <Alert
                            variant="success"
                            className="mt-2"
                            data-testid="google-places-test-ok"
                        >
                            {testStatus.message}
                        </Alert>
                    )}
                    {testStatus.kind === "err" && (
                        <Alert
                            variant="danger"
                            className="mt-2"
                            data-testid="google-places-test-err"
                        >
                            {testStatus.message}
                        </Alert>
                    )}
                </Card.Body>
            </Card>
        </div>
    );
}
