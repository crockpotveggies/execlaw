// Google Places plugin self-contained config panel.
//
// Migrated from `web/src/settings/GooglePlacesConfigPage.tsx` (2026-05-14).
// Build: node scripts/build-plugin-ui.mjs google-places

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface GooglePlacesConfigResponse {
    api_key_set: boolean;
    api_key_masked: string;
    cost_tier: string;
    default_max_results: number;
    validated_at: string;
    validation_error: string;
}

interface GooglePlacesStatusResponse {
    state: string;
    configured: boolean;
    cost_tier: string;
    default_max_results: number;
    validated_at: string;
    validation_error: string;
}

interface GooglePlacesTestResponse {
    ok?: boolean;
    query?: string;
    returned_count?: number;
    first_result_name?: string;
    error?: string;
}

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge } = props;
    const { ErrorBanner, Button } = bridge.components;

    const [config, setConfig] = useState<GooglePlacesConfigResponse | null>(
        null,
    );
    const [status, setStatus] = useState<GooglePlacesStatusResponse | null>(
        null,
    );
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
                bridge.fetchJson<GooglePlacesConfigResponse>(
                    "GET",
                    "/api/admin/plugins/google-places/config",
                ),
                bridge.fetchJson<GooglePlacesStatusResponse>(
                    "GET",
                    "/api/admin/plugins/google-places/status",
                ),
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
    }, [bridge]);

    useEffect(() => {
        void reload();
    }, [reload]);

    const onSave = useCallback(async () => {
        setBusy(true);
        setError(null);
        setSavedNotice(null);
        setTestStatus({ kind: "idle" });
        try {
            const parsedMax =
                defaultMax.trim() === "" ? null : Number(defaultMax);
            if (
                parsedMax !== null &&
                (!Number.isFinite(parsedMax) ||
                    parsedMax < 1 ||
                    parsedMax > 20)
            ) {
                setError("Default max results must be between 1 and 20.");
                setBusy(false);
                return;
            }
            await bridge.fetchJson<{ ok: boolean }>(
                "POST",
                "/api/admin/plugins/google-places/config",
                {
                    api_key: apiKey,
                    cost_tier: costTier,
                    default_max_results: parsedMax ?? "",
                },
            );
            setSavedNotice(
                "Saved. The API key was validated against Google Places.",
            );
            setApiKey("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [apiKey, costTier, defaultMax, bridge, reload]);

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
            const r = await bridge.fetchJson<GooglePlacesTestResponse>(
                "POST",
                "/api/admin/plugins/google-places/test",
                { query: q },
            );
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
    }, [bridge, testQuery]);

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

            <div className="card mb-3">
                <div className="card-body">
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Google Places API</h5>
                        <span
                            className={`badge ${stateBadge}`}
                            data-testid="google-places-status-badge"
                        >
                            {stateLabel}
                        </span>
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
                        . Restrict the key to &quot;Places API (New)&quot; so a leak
                        can&apos;t run up your Maps Geocoding bill.
                    </p>

                    {savedNotice && (
                        <div
                            className="alert alert-success"
                            data-testid="google-places-saved"
                        >
                            {savedNotice}
                        </div>
                    )}
                    {status?.validation_error && (
                        <div className="alert alert-danger">
                            Last validation error:{" "}
                            <code>{status.validation_error}</code>
                        </div>
                    )}

                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            API key
                            {apiKeySet && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config?.api_key_masked}</code>)
                                </span>
                            )}
                        </label>
                        <input
                            type="password"
                            className="form-control"
                            placeholder="AIza..."
                            value={apiKey}
                            onChange={(e: { target: { value: string } }) =>
                                setApiKey(e.target.value)
                            }
                            data-testid="google-places-api-key-input"
                        />
                        <div className="form-text execlaw-muted">
                            Stored locally; never leaves the host. Save validates
                            the key by issuing a 1-result test search before
                            persisting.
                        </div>
                    </div>

                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            Cost tier
                        </label>
                        <select
                            className="form-select"
                            value={costTier}
                            onChange={(e: { target: { value: string } }) =>
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
                        </select>
                        <div className="form-text execlaw-muted">
                            Google Places (New) prices per field-class. Pro
                            covers most agent uses; Essentials is fine when
                            the agent only needs name + address + rating.
                        </div>
                    </div>

                    <div className="mb-3">
                        <label className="form-label execlaw-muted small mb-1">
                            Default max results{" "}
                            <span className="execlaw-muted">(1-20)</span>
                        </label>
                        <input
                            type="number"
                            min={1}
                            max={20}
                            className="form-control"
                            placeholder="5"
                            value={defaultMax}
                            onChange={(e: { target: { value: string } }) =>
                                setDefaultMax(e.target.value)
                            }
                            data-testid="google-places-default-max-input"
                        />
                        <div className="form-text execlaw-muted">
                            Used when an agent&apos;s <code>search</code> call
                            doesn&apos;t specify <code>max_results</code>. Empty
                            falls back to 5.
                        </div>
                    </div>

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
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Test search</h5>
                    <p className="execlaw-muted small mb-2">
                        Issue a sample <code>searchText</code> call. Returns
                        the result count + first place name so you can confirm
                        the key + tier are wired correctly. Costs 1 call
                        against your Google quota.
                    </p>
                    <div className="mb-2">
                        <input
                            type="text"
                            className="form-control"
                            placeholder="coffee near me"
                            value={testQuery}
                            onChange={(e: { target: { value: string } }) =>
                                setTestQuery(e.target.value)
                            }
                            onKeyDown={(e: {
                                key: string;
                                preventDefault: () => void;
                            }) => {
                                if (e.key === "Enter") {
                                    e.preventDefault();
                                    void onTest();
                                }
                            }}
                            data-testid="google-places-test-query-input"
                        />
                    </div>
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
                        <div
                            className="alert alert-success mt-2"
                            data-testid="google-places-test-ok"
                        >
                            {testStatus.message}
                        </div>
                    )}
                    {testStatus.kind === "err" && (
                        <div
                            className="alert alert-danger mt-2"
                            data-testid="google-places-test-err"
                        >
                            {testStatus.message}
                        </div>
                    )}
                </div>
            </div>
        </div>
    );
};

export default Panel;
