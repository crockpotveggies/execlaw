// Plugins page — list installed plugins with enable/disable + a ZIP
// upload form. Phase-2 backend accepts raw application/zip POSTs;
// switching to multipart lands when the upload-progress UI is needed.

import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import {
    disablePlugin,
    enablePlugin,
    installPlugin,
    listPlugins,
    type PluginSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

export function PluginsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [plugins, setPlugins] = useState<PluginSummary[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);

    const fetchList = useCallback(async () => {
        try {
            const r = await listPlugins(getToken);
            setPlugins(r.plugins);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void fetchList();
    }, [fetchList]);

    const onToggle = useCallback(
        async (p: PluginSummary) => {
            setBusyId(p.plugin_id);
            try {
                if (p.enabled) await disablePlugin(p.plugin_id, getToken);
                else await enablePlugin(p.plugin_id, getToken);
                await fetchList();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [fetchList, getToken],
    );

    // Uninstall lives on the per-plugin config page's danger zone
    // now (PluginConfigShell). The row only has gear + toggle.

    return (
        <div data-testid="settings-plugins">
            <h3 className="h6 mb-3">Plugins</h3>

            <InstallCard onInstalled={fetchList} />

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            {plugins === null ? (
                <div className="execlaw-muted small">Loading plugins…</div>
            ) : plugins.length === 0 ? (
                <div className="execlaw-muted small">
                    No plugins installed yet. Drop a ZIP above to get started.
                </div>
            ) : (
                plugins.map((p) => (
                    <div className="execlaw-card" key={p.plugin_id}>
                        <div className="d-flex align-items-center gap-2 mb-2">
                            {/* Title + description stack as a single
                                flex column. `min-width: 0` is what
                                lets the description's
                                nowrap+ellipsis actually take effect
                                inside a flex parent — without it,
                                the column expands to fit the full
                                description string and pushes the
                                version/badge/icons off the row. */}
                            <div className="execlaw-plugin-row__title-col flex-grow-1">
                                {p.has_settings_ui ? (
                                    <Link
                                        to={`/settings/plugins/${encodeURIComponent(p.plugin_id)}`}
                                        className="execlaw-card__title text-decoration-none text-body d-block"
                                        data-testid="plugin-title-link"
                                        data-plugin-id={p.plugin_id}
                                    >
                                        {p.plugin_id}
                                    </Link>
                                ) : (
                                    <span className="execlaw-card__title d-block">
                                        {p.plugin_id}
                                    </span>
                                )}
                                {p.description && (
                                    <div
                                        className="execlaw-plugin-row__desc execlaw-muted small"
                                        title={p.description}
                                        data-testid="plugin-description"
                                    >
                                        {p.description}
                                    </div>
                                )}
                            </div>
                            <span className="execlaw-muted small">
                                v{p.version}
                            </span>
                            <span
                                className={
                                    "execlaw-trust-badge ms-2" +
                                    (p.enabled ? " is-known" : "")
                                }
                            >
                                {p.enabled ? "enabled" : "disabled"}
                            </span>
                            {p.has_settings_ui && (
                                <Link
                                    to={`/settings/plugins/${encodeURIComponent(p.plugin_id)}`}
                                    className="btn btn-sm btn-link p-1 ms-1 text-body"
                                    title="Configure"
                                    aria-label={`Configure ${p.plugin_id}`}
                                    data-testid="plugin-configure"
                                    data-plugin-id={p.plugin_id}
                                >
                                    <i className="bi bi-gear fs-5" aria-hidden />
                                </Link>
                            )}
                            <Button
                                variant="link"
                                size="sm"
                                className="p-1 text-body"
                                disabled={busyId === p.plugin_id}
                                onClick={() => void onToggle(p)}
                                data-testid="plugin-toggle"
                                title={p.enabled ? "Disable plugin" : "Enable plugin"}
                                aria-label={
                                    p.enabled
                                        ? `Disable ${p.plugin_id}`
                                        : `Enable ${p.plugin_id}`
                                }
                                data-enabled={p.enabled}
                            >
                                <i
                                    className={
                                        "bi fs-5 " +
                                        (p.enabled
                                            ? "bi-toggle-on text-success"
                                            : "bi-toggle-off")
                                    }
                                    aria-hidden
                                />
                            </Button>
                        </div>
                    </div>
                ))
            )}
        </div>
    );
}

function InstallCard({ onInstalled }: { onInstalled: () => Promise<void> }) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const fileRef = useRef<HTMLInputElement | null>(null);
    const [installing, setInstalling] = useState(false);
    const [installError, setInstallError] = useState<string | null>(null);
    const [lastInstalled, setLastInstalled] = useState<string | null>(null);

    const onSubmit = useCallback(
        async (e: React.FormEvent<HTMLFormElement>) => {
            e.preventDefault();
            const file = fileRef.current?.files?.[0];
            if (!file) {
                setInstallError("Choose a ZIP file first.");
                return;
            }
            setInstalling(true);
            setInstallError(null);
            try {
                const r = await installPlugin(file, getToken);
                setLastInstalled(`${r.plugin_id} v${r.version}`);
                if (fileRef.current) fileRef.current.value = "";
                await onInstalled();
            } catch (e) {
                setInstallError(e instanceof Error ? e.message : String(e));
            } finally {
                setInstalling(false);
            }
        },
        [getToken, onInstalled],
    );

    return (
        <div className="execlaw-card mb-3">
            <div className="execlaw-card__title">Install a plugin</div>
            <Form onSubmit={onSubmit} className="d-flex gap-2 align-items-end">
                <Form.Group className="flex-grow-1">
                    <Form.Label className="execlaw-muted small mb-1">
                        ZIP archive
                    </Form.Label>
                    <Form.Control
                        ref={fileRef}
                        type="file"
                        accept=".zip,application/zip"
                        disabled={installing}
                        data-testid="plugin-install-file"
                    />
                </Form.Group>
                <Button
                    type="submit"
                    variant="primary"
                    disabled={installing}
                    data-testid="plugin-install-submit"
                >
                    {installing ? (
                        <Spinner size="sm" animation="border" />
                    ) : (
                        <>
                            <i className="bi bi-upload me-2" aria-hidden />
                            Install
                        </>
                    )}
                </Button>
            </Form>
            <ErrorBanner message={installError} onDismiss={() => setInstallError(null)} className="mt-2" />
            {lastInstalled && !installError && (
                <div className="execlaw-muted small mt-2">
                    Installed {lastInstalled}.
                </div>
            )}
        </div>
    );
}
