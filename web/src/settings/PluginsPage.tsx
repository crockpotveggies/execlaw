// Plugins page — list installed plugins with enable/disable + a ZIP
// upload form. Phase-2 backend accepts raw application/zip POSTs;
// switching to multipart lands when the upload-progress UI is needed.

import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";
import {
    disablePlugin,
    enablePlugin,
    installBundledPlugin,
    installPlugin,
    listBundledPlugins,
    listPlugins,
    type BundledPlugin,
    type PluginSummary,
} from "../api/endpoints";
import { ApiError } from "../api/client";
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

    // Two install affordances: a modal browser over the bundled
    // ZIPs the .app shipped (or the operator dropped into
    // ~/.execlaw/bundled-plugins/) and a file-picker for ZIPs from
    // elsewhere. We default to the file input HIDDEN so the panel
    // reads as "choose one of these two paths" rather than
    // "here's a file input plus a button that doesn't look as
    // important." The file form is revealed on demand.
    const [showFileForm, setShowFileForm] = useState(false);
    const [showBundledModal, setShowBundledModal] = useState(false);
    // Bundled-plugin count drives the button label
    // ("Browse 13 bundled plugins") and lets us hide the affordance
    // entirely when the host has none. Fetched once on mount;
    // refreshed by the modal each time it opens.
    const [bundledCount, setBundledCount] = useState<number | null>(null);

    const refreshBundledCount = useCallback(async () => {
        try {
            const r = await listBundledPlugins(getToken);
            setBundledCount(r.plugins.length);
        } catch {
            // Endpoint missing on older server builds → bundled
            // affordance hides via the count=null/0 branch below.
            setBundledCount(0);
        }
    }, [getToken]);

    useEffect(() => {
        void refreshBundledCount();
    }, [refreshBundledCount]);

    const runInstall = useCallback(
        async (
            file: File,
            ifExisting: "reject" | "upgrade",
        ): Promise<void> => {
            const r = await installPlugin(file, getToken, ifExisting);
            const verb = ifExisting === "upgrade" ? "Upgraded" : "Installed";
            setLastInstalled(`${verb} ${r.plugin_id} v${r.version}`);
            if (fileRef.current) fileRef.current.value = "";
            await onInstalled();
        },
        [getToken, onInstalled],
    );

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
                await runInstall(file, "reject");
            } catch (err) {
                // 409 means a plugin with the same id is already
                // installed. Ask the operator to confirm an upgrade,
                // then retry with `if_existing=upgrade` so the
                // backend tears down the old runtime and replaces it
                // (per-plugin OAuth client + tokens survive the
                // upgrade — they live in `state_oauth_*`, not
                // `state_plugins`).
                const isConflict =
                    err instanceof ApiError && err.code === "conflict";
                if (isConflict) {
                    const ok = window.confirm(
                        "A plugin with this id is already installed. " +
                            "Replace it with the new ZIP? Your OAuth " +
                            "connection (client + granted tokens) " +
                            "will be preserved.",
                    );
                    if (!ok) {
                        setInstallError("Install cancelled.");
                    } else {
                        try {
                            await runInstall(file, "upgrade");
                        } catch (err2) {
                            setInstallError(
                                err2 instanceof Error
                                    ? err2.message
                                    : String(err2),
                            );
                        }
                    }
                } else {
                    setInstallError(
                        err instanceof Error ? err.message : String(err),
                    );
                }
            } finally {
                setInstalling(false);
            }
        },
        [runInstall],
    );

    return (
        <div className="execlaw-card mb-3">
            <div className="execlaw-card__title">Install a plugin</div>
            <div className="execlaw-muted small mb-3">
                Pick from the plugins this build shipped with, or upload a
                ZIP you downloaded from elsewhere.
            </div>
            <div className="d-flex flex-wrap gap-2">
                <Button
                    variant="primary"
                    onClick={() => setShowBundledModal(true)}
                    disabled={bundledCount === 0}
                    data-testid="plugin-install-browse-bundled"
                >
                    <i className="bi bi-collection me-2" aria-hidden />
                    Browse bundled plugins
                    {bundledCount !== null && bundledCount > 0 && (
                        <span className="execlaw-muted small ms-2">
                            ({bundledCount})
                        </span>
                    )}
                </Button>
                <Button
                    variant={showFileForm ? "secondary" : "outline-secondary"}
                    onClick={() => setShowFileForm((v) => !v)}
                    data-testid="plugin-install-toggle-file"
                >
                    <i className="bi bi-upload me-2" aria-hidden />
                    From file…
                </Button>
            </div>
            {showFileForm && (
                <Form
                    onSubmit={onSubmit}
                    className="d-flex gap-2 align-items-end mt-3"
                >
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
                                <i className="bi bi-check2 me-2" aria-hidden />
                                Install
                            </>
                        )}
                    </Button>
                </Form>
            )}
            <ErrorBanner
                message={installError}
                onDismiss={() => setInstallError(null)}
                className="mt-2"
            />
            {lastInstalled && !installError && (
                <div className="execlaw-muted small mt-2">
                    Installed {lastInstalled}.
                </div>
            )}
            <BundledPluginsModal
                show={showBundledModal}
                onHide={() => setShowBundledModal(false)}
                onInstalled={async () => {
                    await onInstalled();
                    await refreshBundledCount();
                }}
            />
        </div>
    );
}

/// Modal listing every bundled plugin ZIP with one-click install
/// buttons. Sourced from `~/.execlaw/bundled-plugins/`, which is
/// populated either by the macOS .app's boot-time mirror or by an
/// operator dropping ZIPs in by hand.
///
/// The native HTML file picker has no API for setting an initial
/// directory — even Tauri's `<input type=file>` defers to the OS
/// chooser, which goes wherever the OS last remembered. Surfacing
/// the available bundled ZIPs directly here avoids that fight
/// entirely: the operator picks from a curated list instead of
/// hunting for a ZIP in Finder.
function BundledPluginsModal({
    show,
    onHide,
    onInstalled,
}: {
    show: boolean;
    onHide: () => void;
    onInstalled: () => Promise<void>;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [items, setItems] = useState<BundledPlugin[] | null>(null);
    const [busyFile, setBusyFile] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [lastInstalled, setLastInstalled] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listBundledPlugins(getToken);
            setItems(r.plugins);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setItems([]);
        }
    }, [getToken]);

    // Re-fetch whenever the modal opens so the `already_installed`
    // badges reflect the latest server state. Skip when the modal
    // is hidden to avoid wasted requests.
    useEffect(() => {
        if (show) {
            void refresh();
        }
    }, [show, refresh]);

    const runInstall = useCallback(
        async (entry: BundledPlugin) => {
            setBusyFile(entry.file);
            setError(null);
            try {
                // Always try `reject` first; on 409 prompt the
                // operator to confirm and retry with `upgrade`.
                // Mirrors the upload-path UX in `InstallCard`.
                try {
                    const r = await installBundledPlugin(
                        entry.file,
                        getToken,
                        "reject",
                    );
                    setLastInstalled(`${r.plugin_id} v${r.version}`);
                } catch (err) {
                    const isConflict =
                        err instanceof ApiError && err.code === "conflict";
                    if (isConflict) {
                        const ok = window.confirm(
                            `${entry.plugin_id ?? entry.file} is already installed. ` +
                                "Replace it with this bundled ZIP? OAuth tokens + " +
                                "plugin-specific config survive the upgrade.",
                        );
                        if (!ok) {
                            setError("Install cancelled.");
                            return;
                        }
                        const r = await installBundledPlugin(
                            entry.file,
                            getToken,
                            "upgrade",
                        );
                        setLastInstalled(`${r.plugin_id} v${r.version}`);
                    } else {
                        throw err;
                    }
                }
                await onInstalled();
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyFile(null);
            }
        },
        [getToken, onInstalled, refresh],
    );

    return (
        <Modal
            show={show}
            onHide={onHide}
            size="lg"
            scrollable
            data-testid="plugins-bundled-modal"
        >
            <Modal.Header closeButton>
                <Modal.Title className="h6">
                    <i className="bi bi-collection me-2" aria-hidden />
                    Bundled plugins
                </Modal.Title>
            </Modal.Header>
            <Modal.Body>
                <div className="execlaw-muted small mb-3">
                    These ZIPs ship with this build of execlaw under{" "}
                    <code>~/.execlaw/bundled-plugins/</code>. Click Install on
                    any row to add the plugin without searching for the file.
                </div>
                <ErrorBanner
                    message={error}
                    onDismiss={() => setError(null)}
                    className="mb-2"
                />
                {lastInstalled && !error && (
                    <div className="execlaw-muted small mb-2">
                        Installed {lastInstalled}.
                    </div>
                )}
                {items === null ? (
                    <div className="execlaw-muted small">
                        Loading bundled plugins…
                    </div>
                ) : items.length === 0 ? (
                    <div className="execlaw-muted small">
                        No bundled plugins available on this host. Drop ZIPs
                        into <code>~/.execlaw/bundled-plugins/</code> and
                        reopen, or use <strong>From file…</strong> below to
                        upload one directly.
                    </div>
                ) : (
                    items.map((p) => (
                        <div
                            key={p.file}
                            className="d-flex align-items-center gap-2 py-2 border-bottom"
                            data-testid="plugins-bundled-row"
                        >
                            <div className="flex-grow-1 min-w-0">
                                <div className="d-flex align-items-baseline gap-2 flex-wrap">
                                    <strong>{p.plugin_id ?? p.file}</strong>
                                    {p.version && (
                                        <span className="execlaw-muted small">
                                            v{p.version}
                                        </span>
                                    )}
                                    <span className="execlaw-muted small">
                                        · {Math.max(1, Math.round(p.size_bytes / 1024))} KB
                                    </span>
                                    {p.already_installed && (
                                        <span
                                            className="execlaw-trust-badge is-known"
                                            title="A plugin with this id is already installed"
                                        >
                                            installed
                                        </span>
                                    )}
                                </div>
                                {p.description && (
                                    <div className="execlaw-muted small">
                                        {p.description}
                                    </div>
                                )}
                            </div>
                            <Button
                                variant={
                                    p.already_installed
                                        ? "outline-secondary"
                                        : "primary"
                                }
                                size="sm"
                                onClick={() => void runInstall(p)}
                                disabled={busyFile !== null}
                                data-testid="plugins-bundled-install"
                            >
                                {busyFile === p.file ? (
                                    <Spinner size="sm" animation="border" />
                                ) : p.already_installed ? (
                                    <>
                                        <i
                                            className="bi bi-arrow-repeat me-1"
                                            aria-hidden
                                        />
                                        Reinstall
                                    </>
                                ) : (
                                    <>
                                        <i
                                            className="bi bi-download me-1"
                                            aria-hidden
                                        />
                                        Install
                                    </>
                                )}
                            </Button>
                        </div>
                    ))
                )}
            </Modal.Body>
            <Modal.Footer>
                <Button variant="secondary" onClick={onHide}>
                    Close
                </Button>
            </Modal.Footer>
        </Modal>
    );
}
