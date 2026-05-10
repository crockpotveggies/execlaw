// Settings → Plugins → Pushover. Operator pastes their user_key
// + app_token here; the plugin persists them via its per-plugin
// vault scope and uses them at agent-tool dispatch time.
//
// Two affordances:
//   * Save form — write the keys back to the plugin (server
//     masks the values on read so refreshing the page doesn't
//     leak the originals into the SPA).
//   * Test button — fires a one-shot notification end-to-end so
//     the operator confirms the keys actually work before the
//     agent's first call.

import { useCallback, useEffect, useState, type JSX } from "react";
import { Alert, Badge, Button, Card, Form, Spinner } from "react-bootstrap";
import {
    getPushoverConfig,
    setPushoverConfig,
    testPushoverNotification,
    type PushoverConfigResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

export function PushoverConfigPage(_props: PluginConfigProps): JSX.Element {
    const { getAccessToken } = useAuth();
    const [config, setConfig] = useState<PushoverConfigResponse | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [userKey, setUserKey] = useState("");
    const [appToken, setAppToken] = useState("");
    const [savedNotice, setSavedNotice] = useState<string | null>(null);
    const [testStatus, setTestStatus] = useState<
        | { kind: "idle" }
        | { kind: "ok"; message: string }
        | { kind: "err"; message: string }
    >({ kind: "idle" });

    const reload = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const r = await getPushoverConfig(getAccessToken);
            setConfig(r);
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
            await setPushoverConfig(userKey, appToken, getAccessToken);
            setSavedNotice("Saved. Inputs are now empty — the keys live in the plugin's vault; the masked tail above is what the SPA reads back.");
            setUserKey("");
            setAppToken("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [appToken, getAccessToken, reload, userKey]);

    const onTest = useCallback(async () => {
        setBusy(true);
        setTestStatus({ kind: "idle" });
        setError(null);
        try {
            const r = await testPushoverNotification(getAccessToken);
            if (r.ok) {
                setTestStatus({
                    kind: "ok",
                    message: `Sent. Pushover request id: ${r.request_id ?? "(none)"}`,
                });
            } else {
                setTestStatus({
                    kind: "err",
                    message: r.error ?? "Pushover rejected the test notification.",
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

    const userKeySet = config?.user_key_set ?? false;
    const tokenSet = config?.app_token_set ?? false;
    const fullyConfigured = userKeySet && tokenSet;

    return (
        <div data-testid="pushover-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <Card className="mb-3">
                <Card.Body>
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Pushover credentials</h5>
                        {fullyConfigured ? (
                            <Badge bg="success" data-testid="pushover-status">
                                Configured
                            </Badge>
                        ) : (
                            <Badge bg="warning" text="dark" data-testid="pushover-status">
                                Incomplete
                            </Badge>
                        )}
                    </div>
                    <p className="execlaw-muted small mb-3">
                        Get the <strong>User Key</strong> from your Pushover
                        account dashboard at{" "}
                        <a
                            href="https://pushover.net/"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            pushover.net
                        </a>{" "}
                        (top-right). Create an{" "}
                        <strong>Application / API Token</strong> at{" "}
                        <a
                            href="https://pushover.net/apps/build"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            pushover.net/apps/build
                        </a>{" "}
                        — name it &quot;execlaw&quot;, the icon is your call.
                        Paste both below and Save.
                    </p>

                    {savedNotice && (
                        <Alert variant="success" data-testid="pushover-saved">
                            {savedNotice}
                        </Alert>
                    )}

                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            User key
                            {userKeySet && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config?.user_key_masked}</code>)
                                </span>
                            )}
                        </Form.Label>
                        <Form.Control
                            type="password"
                            placeholder="u-..."
                            value={userKey}
                            onChange={(e) => setUserKey(e.target.value)}
                            data-testid="pushover-user-key-input"
                        />
                        <Form.Text className="execlaw-muted">
                            30-char Pushover user identifier. Leave blank to keep the existing value;
                            paste anything to replace it.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Application token
                            {tokenSet && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config?.app_token_masked}</code>)
                                </span>
                            )}
                        </Form.Label>
                        <Form.Control
                            type="password"
                            placeholder="a-..."
                            value={appToken}
                            onChange={(e) => setAppToken(e.target.value)}
                            data-testid="pushover-token-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Per-application API token. Each app you build at
                            pushover.net gets its own token; one for execlaw is fine.
                        </Form.Text>
                    </Form.Group>

                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={
                                busy || (userKey.trim() === "" && appToken.trim() === "")
                            }
                            data-testid="pushover-save"
                        >
                            Save
                        </Button>
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={() => void onTest()}
                            disabled={busy || !fullyConfigured}
                            data-testid="pushover-test"
                        >
                            Send test notification
                        </Button>
                    </div>
                </Card.Body>
            </Card>

            {testStatus.kind === "ok" && (
                <Alert variant="success" data-testid="pushover-test-ok">
                    {testStatus.message}
                </Alert>
            )}
            {testStatus.kind === "err" && (
                <Alert variant="danger" data-testid="pushover-test-err">
                    {testStatus.message}
                </Alert>
            )}
        </div>
    );
}
