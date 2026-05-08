// Settings → Plugins → SMS Socket.
//
// The operator runs the sms-socket-app on an Android phone, copies
// the API key + gateway URL out of the app, and pastes them here.
// On save the plugin persists them to its per-plugin vault scope.
//
// Three affordances:
//   * Save form — write api_key + gateway_url + optional
//     default_subscription_id back to the plugin (server masks the
//     api_key on read so refreshing doesn't leak the original).
//   * Status card — surfaces the most recent gateway.state ping
//     (running / enabled / connection count / addresses) plus the
//     outbox depth so wiring problems are visible.
//   * Test button — queues a one-shot SMS so the operator confirms
//     the round-trip works end-to-end.
//
// Tool calls reach the WS via the host's per-plugin "active bidi
// handle" slot (set by ws_subscribe_bidi, read by ws_send_to_active).
// Sends are immediate, with no vault-backed outbox in between, so
// concurrent tool calls are safe under the WS handle's mpsc.

import { useCallback, useEffect, useState } from "react";
import { Alert, Badge, Button, Card, Form, Spinner } from "react-bootstrap";
import {
    getSmsSocketConfig,
    getSmsSocketStatus,
    setSmsSocketConfig,
    testSmsSocketMessage,
    type SmsSocketConfigResponse,
    type SmsSocketStatusResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

interface GatewayStatePayload {
    running?: boolean;
    enabled?: boolean;
    addresses?: string[];
    connectionCount?: number;
    apiKeyPreview?: string;
}

export function SmsSocketConfigPage(_props: PluginConfigProps): JSX.Element {
    const { getAccessToken } = useAuth();
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
                getSmsSocketConfig(getAccessToken),
                getSmsSocketStatus(getAccessToken),
            ]);
            setConfig(c);
            setStatus(s);
            // Pre-fill the gateway-url + subscription-id inputs with
            // the existing values so the operator can edit in place.
            // The api_key stays empty (it's a secret; we only show
            // the masked tail in the label).
            setGatewayUrl(c.gateway_url);
            setSubscriptionId(c.default_subscription_id);
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
            await setSmsSocketConfig(
                apiKey,
                gatewayUrl.trim(),
                subscriptionId.trim(),
                getAccessToken,
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
    }, [apiKey, gatewayUrl, getAccessToken, reload, subscriptionId]);

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
            const r = await testSmsSocketMessage(to, getAccessToken);
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
    }, [getAccessToken, reload, testTo]);

    if (loading) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <Spinner animation="border" size="sm" className="me-2" />
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

            <Card className="mb-3">
                <Card.Body>
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">SMS gateway credentials</h5>
                        {configured ? (
                            <Badge bg="success" data-testid="sms-socket-status">
                                Configured
                            </Badge>
                        ) : (
                            <Badge
                                bg="warning"
                                text="dark"
                                data-testid="sms-socket-status"
                            >
                                Unconfigured
                            </Badge>
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
                        with the phone's LAN IP). For TLS, use{" "}
                        <code>wss://</code>.
                    </p>

                    {savedNotice && (
                        <Alert variant="success" data-testid="sms-socket-saved">
                            {savedNotice}
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
                            placeholder="paste the gateway's API key"
                            value={apiKey}
                            onChange={(e) => setApiKey(e.target.value)}
                            data-testid="sms-socket-api-key-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Sent as <code>Authorization: Bearer …</code> on the
                            WebSocket upgrade. Leave blank to keep the existing
                            value; paste anything to replace it.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Gateway URL
                        </Form.Label>
                        <Form.Control
                            type="text"
                            placeholder="ws://127.0.0.1:8787/"
                            value={gatewayUrl}
                            onChange={(e) => setGatewayUrl(e.target.value)}
                            data-testid="sms-socket-url-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Must start with <code>ws://</code> or{" "}
                            <code>wss://</code>. Default port in the app is{" "}
                            <code>8787</code>.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Default SIM subscription id{" "}
                            <span className="execlaw-muted">(optional)</span>
                        </Form.Label>
                        <Form.Control
                            type="text"
                            placeholder="leave blank for the phone's default SIM"
                            value={subscriptionId}
                            onChange={(e) => setSubscriptionId(e.target.value)}
                            data-testid="sms-socket-sub-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Integer Android subscription id. Only needed on
                            dual-SIM phones to pin sends to a specific line.
                        </Form.Text>
                    </Form.Group>

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
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
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
                                    <Badge
                                        bg={running ? "success" : "secondary"}
                                        data-testid="sms-socket-running"
                                    >
                                        {running ? "running" : "no recent ping"}
                                    </Badge>
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
                                            (addr, i) => (
                                                <code key={addr} className="ms-1">
                                                    {addr}
                                                    {i <
                                                    (gatewayState.addresses?.length ??
                                                        0) -
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
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Test message</h5>
                    <p className="execlaw-muted small mb-2">
                        Sends a one-shot SMS through the gateway so you can
                        confirm wiring end-to-end. The send is queued in the
                        plugin's outbox and flushes on the next inbound frame
                        — typically within a few seconds.
                    </p>
                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Recipient (E.164)
                        </Form.Label>
                        <Form.Control
                            type="text"
                            placeholder="+14165550100"
                            value={testTo}
                            onChange={(e) => setTestTo(e.target.value)}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                    e.preventDefault();
                                    void onTest();
                                }
                            }}
                            data-testid="sms-socket-test-to-input"
                        />
                    </Form.Group>
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
                        <Alert
                            variant="success"
                            className="mt-2"
                            data-testid="sms-socket-test-ok"
                        >
                            {testStatus.message}
                        </Alert>
                    )}
                    {testStatus.kind === "err" && (
                        <Alert
                            variant="danger"
                            className="mt-2"
                            data-testid="sms-socket-test-err"
                        >
                            {testStatus.message}
                        </Alert>
                    )}
                </Card.Body>
            </Card>
        </div>
    );
}
