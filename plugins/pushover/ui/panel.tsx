// Pushover plugin self-contained config panel.
//
// Migrated from `web/src/settings/PushoverConfigPage.tsx` (2026-05-14).
// Build: node scripts/build-plugin-ui.mjs pushover

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface PushoverConfigResponse {
    user_key_set: boolean;
    user_key_masked: string;
    app_token_set: boolean;
    app_token_masked: string;
}

interface PushoverTestResponse {
    ok?: boolean;
    request_id?: string;
    error?: string;
}

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge } = props;
    const { ErrorBanner, Button } = bridge.components;

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
            const r = await bridge.fetchJson<PushoverConfigResponse>(
                "GET",
                "/api/admin/plugins/pushover/config",
            );
            setConfig(r);
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
            await bridge.fetchJson<unknown>(
                "POST",
                "/api/admin/plugins/pushover/config",
                { user_key: userKey, app_token: appToken },
            );
            setSavedNotice(
                "Saved. Inputs are now empty — the keys live in the plugin's vault; the masked tail above is what the SPA reads back.",
            );
            setUserKey("");
            setAppToken("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [appToken, bridge, reload, userKey]);

    const onTest = useCallback(async () => {
        setBusy(true);
        setTestStatus({ kind: "idle" });
        setError(null);
        try {
            const r = await bridge.fetchJson<PushoverTestResponse>(
                "POST",
                "/api/admin/plugins/pushover/test",
            );
            if (r.ok) {
                setTestStatus({
                    kind: "ok",
                    message: `Sent. Pushover request id: ${r.request_id ?? "(none)"}`,
                });
            } else {
                setTestStatus({
                    kind: "err",
                    message:
                        r.error ?? "Pushover rejected the test notification.",
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

            <div className="card mb-3">
                <div className="card-body">
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Pushover credentials</h5>
                        {fullyConfigured ? (
                            <span
                                className="badge bg-success"
                                data-testid="pushover-status"
                            >
                                Configured
                            </span>
                        ) : (
                            <span
                                className="badge bg-warning text-dark"
                                data-testid="pushover-status"
                            >
                                Incomplete
                            </span>
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
                        <div className="alert alert-success" data-testid="pushover-saved">
                            {savedNotice}
                        </div>
                    )}

                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            User key
                            {userKeySet && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config?.user_key_masked}</code>)
                                </span>
                            )}
                        </label>
                        <input
                            type="password"
                            className="form-control"
                            placeholder="u-..."
                            value={userKey}
                            onChange={(e: { target: { value: string } }) =>
                                setUserKey(e.target.value)
                            }
                            data-testid="pushover-user-key-input"
                        />
                        <div className="form-text execlaw-muted">
                            30-char Pushover user identifier. Leave blank to keep the existing value;
                            paste anything to replace it.
                        </div>
                    </div>

                    <div className="mb-3">
                        <label className="form-label execlaw-muted small mb-1">
                            Application token
                            {tokenSet && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config?.app_token_masked}</code>)
                                </span>
                            )}
                        </label>
                        <input
                            type="password"
                            className="form-control"
                            placeholder="a-..."
                            value={appToken}
                            onChange={(e: { target: { value: string } }) =>
                                setAppToken(e.target.value)
                            }
                            data-testid="pushover-token-input"
                        />
                        <div className="form-text execlaw-muted">
                            Per-application API token. Each app you build at
                            pushover.net gets its own token; one for execlaw is fine.
                        </div>
                    </div>

                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={
                                busy ||
                                (userKey.trim() === "" && appToken.trim() === "")
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
                </div>
            </div>

            {testStatus.kind === "ok" && (
                <div className="alert alert-success" data-testid="pushover-test-ok">
                    {testStatus.message}
                </div>
            )}
            {testStatus.kind === "err" && (
                <div className="alert alert-danger" data-testid="pushover-test-err">
                    {testStatus.message}
                </div>
            )}
        </div>
    );
};

export default Panel;
