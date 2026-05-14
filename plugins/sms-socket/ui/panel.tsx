// SMS Socket plugin self-contained config panel.
//
// Migrated from `web/src/settings/SmsSocketConfigPage.tsx` (2026-05-14).
// Build: node scripts/build-plugin-ui.mjs sms-socket

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface SmsSocketConfigResponse {
    api_key_set: boolean;
    api_key_masked: string;
    gateway_url: string;
    default_subscription_id: string;
}

interface SmsSocketStatusResponse {
    sidecar_status: string;
    sidecar_rpc_url: string | null;
    gateway_url: string;
    configured: boolean;
    gateway_state: unknown;
}

interface SmsSocketTestResponse {
    ok?: boolean;
    request_id?: string;
    note?: string;
    error?: string;
}

interface GatewayStatePayload {
    running?: boolean;
    enabled?: boolean;
    addresses?: string[];
    connectionCount?: number;
    apiKeyPreview?: string;
}

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge } = props;
    const { ErrorBanner, Button } = bridge.components;

    const [config, setConfig] = useState<SmsSocketConfigResponse | null>(null);
    const [status, setStatus] = useState<SmsSocketStatusResponse | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [apiKey, setApiKey] = useState("");
    const [gatewayUrl, setGatewayUrl] = useState("");
    const [subscriptionId, setSubscriptionId] = useState("");
    const [savedNotice, setSavedNotice] = useState<string | null>(null);
    const [testTo, setTestTo] = useState("");
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
                bridge.fetchJson<SmsSocketConfigResponse>(
                    "GET",
                    "/api/admin/plugins/sms-socket/config",
                ),
                bridge.fetchJson<SmsSocketStatusResponse>(
                    "GET",
                    "/api/admin/plugins/sms-socket/status",
                ),
            ]);
            setConfig(c);
            setStatus(s);
            setGatewayUrl(c.gateway_url);
            setSubscriptionId(c.default_subscription_id);
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
            await bridge.fetchJson<{ ok: boolean; reconnected?: boolean }>(
                "POST",
                "/api/admin/plugins/sms-socket/config",
                {
                    api_key: apiKey,
                    gateway_url: gatewayUrl.trim(),
                    default_subscription_id: subscriptionId.trim(),
                },
            );
            setSavedNotice(
                "Saved. The plugin tore down its old WebSocket and reconnected with the new credentials — check the gateway-status panel below for the next ping.",
            );
            setApiKey("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [apiKey, gatewayUrl, bridge, reload, subscriptionId]);

    const onTest = useCallback(async () => {
        const to = testTo.trim();
        if (to === "") {
            setTestStatus({
                kind: "err",
                message: "Phone number required (E.164, e.g. +14165550100).",
            });
            return;
        }
        setBusy(true);
        setTestStatus({ kind: "idle" });
        setError(null);
        try {
            const r = await bridge.fetchJson<SmsSocketTestResponse>(
                "POST",
                "/api/admin/plugins/sms-socket/test",
                { to },
            );
            if (r.ok === false) {
                setTestStatus({
                    kind: "err",
                    message: r.error ?? "Gateway rejected the test message.",
                });
            } else {
                setTestStatus({
                    kind: "ok",
                    message:
                        r.note ??
                        `Queued. Request id: ${r.request_id ?? "(none)"}.`,
                });
            }
            await reload();
        } catch (e) {
            setTestStatus({
                kind: "err",
                message: e instanceof Error ? e.message : String(e),
            });
        } finally {
            setBusy(false);
        }
    }, [bridge, reload, testTo]);

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
    const configured = apiKeySet;
    const gatewayState =
        (status?.gateway_state as GatewayStatePayload | null | undefined) ??
        null;
    const running = gatewayState?.running === true;

    return (
        <div data-testid="sms-socket-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <div className="card mb-3">
                <div className="card-body">
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">SMS gateway credentials</h5>
                        {configured ? (
                            <span
                                className="badge bg-success"
                                data-testid="sms-socket-status"
                            >
                                Configured
                            </span>
                        ) : (
                            <span
                                className="badge bg-warning text-dark"
                                data-testid="sms-socket-status"
                            >
                                Unconfigured
                            </span>
                        )}
                    </div>
                    <p className="execlaw-muted small mb-3">
                        Install the{" "}
                        <a
                            href="https://github.com/crockpotveggies/sms-socket-app"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            sms-socket-app
                        </a>{" "}
                        on your Android phone, start the gateway, and copy the
                        generated API key. The default URL{" "}
                        <code>ws://127.0.0.1:8787/</code> works when the phone
                        is reachable from this host (USB tether with{" "}
                        <code>adb reverse tcp:8787 tcp:8787</code>, or Wi-Fi
                        with the phone&apos;s LAN IP). For TLS, use{" "}
                        <code>wss://</code>.
                    </p>

                    {savedNotice && (
                        <div
                            className="alert alert-success"
                            data-testid="sms-socket-saved"
                        >
                            {savedNotice}
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
                            placeholder="paste the gateway's API key"
                            value={apiKey}
                            onChange={(e: { target: { value: string } }) =>
                                setApiKey(e.target.value)
                            }
                            data-testid="sms-socket-api-key-input"
                        />
                        <div className="form-text execlaw-muted">
                            Sent as <code>Authorization: Bearer …</code> on the
                            WebSocket upgrade. Leave blank to keep the existing
                            value; paste anything to replace it.
                        </div>
                    </div>

                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            Gateway URL
                        </label>
                        <input
                            type="text"
                            className="form-control"
                            placeholder="ws://127.0.0.1:8787/"
                            value={gatewayUrl}
                            onChange={(e: { target: { value: string } }) =>
                                setGatewayUrl(e.target.value)
                            }
                            data-testid="sms-socket-url-input"
                        />
                        <div className="form-text execlaw-muted">
                            Must start with <code>ws://</code> or{" "}
                            <code>wss://</code>. Default port in the app is{" "}
                            <code>8787</code>.
                        </div>
                    </div>

                    <div className="mb-3">
                        <label className="form-label execlaw-muted small mb-1">
                            Default SIM subscription id{" "}
                            <span className="execlaw-muted">(optional)</span>
                        </label>
                        <input
                            type="text"
                            className="form-control"
                            placeholder="leave blank for the phone's default SIM"
                            value={subscriptionId}
                            onChange={(e: { target: { value: string } }) =>
                                setSubscriptionId(e.target.value)
                            }
                            data-testid="sms-socket-sub-input"
                        />
                        <div className="form-text execlaw-muted">
                            Integer Android subscription id. Only needed on
                            dual-SIM phones to pin sends to a specific line.
                        </div>
                    </div>

                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={busy}
                            data-testid="sms-socket-save"
                        >
                            Save
                        </Button>
                    </div>
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Gateway status</h5>
                    {!configured ? (
                        <p className="execlaw-muted small mb-0">
                            Configure the credentials above first; status pings
                            land here once the WebSocket connects.
                        </p>
                    ) : (
                        <>
                            <div className="d-flex flex-wrap gap-3 align-items-center mb-2">
                                <span>
                                    Connection:{" "}
                                    <span
                                        className={
                                            "badge " +
                                            (running
                                                ? "bg-success"
                                                : "bg-secondary")
                                        }
                                        data-testid="sms-socket-running"
                                    >
                                        {running ? "running" : "no recent ping"}
                                    </span>
                                </span>
                                {gatewayState?.connectionCount !== undefined && (
                                    <span className="execlaw-muted small">
                                        Active sockets:{" "}
                                        {gatewayState.connectionCount}
                                    </span>
                                )}
                            </div>
                            {gatewayState?.addresses &&
                                gatewayState.addresses.length > 0 && (
                                    <div className="execlaw-muted small mb-2">
                                        Phone reports listen addresses:{" "}
                                        {gatewayState.addresses.map(
                                            (addr: string, i: number) => (
                                                <code
                                                    key={addr}
                                                    className="ms-1"
                                                >
                                                    {addr}
                                                    {i <
                                                    (gatewayState.addresses
                                                        ?.length ?? 0) -
                                                        1
                                                        ? ","
                                                        : ""}
                                                </code>
                                            ),
                                        )}
                                    </div>
                                )}
                            {!gatewayState && (
                                <p className="execlaw-muted small mb-0">
                                    No gateway.state ping seen yet. The
                                    Android app emits one every few seconds —
                                    if this stays empty, double-check the URL
                                    and that the plugin is enabled.
                                </p>
                            )}
                        </>
                    )}
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Test message</h5>
                    <p className="execlaw-muted small mb-2">
                        Sends a one-shot SMS through the gateway so you can
                        confirm wiring end-to-end. The send is queued in the
                        plugin&apos;s outbox and flushes on the next inbound frame
                        — typically within a few seconds.
                    </p>
                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            Recipient (E.164)
                        </label>
                        <input
                            type="text"
                            className="form-control"
                            placeholder="+14165550100"
                            value={testTo}
                            onChange={(e: { target: { value: string } }) =>
                                setTestTo(e.target.value)
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
                            data-testid="sms-socket-test-to-input"
                        />
                    </div>
                    <Button
                        size="sm"
                        variant="outline-secondary"
                        onClick={() => void onTest()}
                        disabled={busy || !configured}
                        data-testid="sms-socket-test"
                    >
                        Send test SMS
                    </Button>
                    {testStatus.kind === "ok" && (
                        <div
                            className="alert alert-success mt-2"
                            data-testid="sms-socket-test-ok"
                        >
                            {testStatus.message}
                        </div>
                    )}
                    {testStatus.kind === "err" && (
                        <div
                            className="alert alert-danger mt-2"
                            data-testid="sms-socket-test-err"
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
