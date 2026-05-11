// Per-plugin config page shell. URL: `/settings/plugins/:plugin_id`.
//
// Architecture:
//
//   PluginConfigShell (THIS file — owns the chrome: header + danger zone)
//     ├─ <Component {...PluginConfigProps} />   ← plugin's middle content
//     └─ DangerZone (Uninstall) — unconditional, plugin can't override
//
// Plugin authors implement a `(props: PluginConfigProps) => JSX.Element`
// against the contract in `PluginConfigBase.tsx`. They render
// whatever they want INSIDE the shell; they don't get to skip
// the danger zone, render their own back button, or change the
// "Uninstall" affordance. That keeps the plugin lifecycle UI
// consistent across plugins and prevents a buggy/malicious
// plugin from hiding its own uninstall.
//
// When the schema-driven `[[settings_fields]]` mechanism lands,
// the fallback case here becomes a generic form renderer — the
// shell stays unchanged.

import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";
import {
    factoryResetPlugin,
    listPlugins,
    uninstallPlugin,
    type FactoryResetPluginResponse,
    type PluginSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { GoogleCalendarPage } from "./GoogleCalendarPage";
import { GoogleContactsPage } from "./GoogleContactsPage";
import { GooglePlacesConfigPage } from "./GooglePlacesConfigPage";
import type { PluginConfigComponent } from "./PluginConfigBase";
import { PushoverConfigPage } from "./PushoverConfigPage";
import { SignalConfigPage } from "./SignalConfigPage";
import { SlackConfigPage } from "./SlackConfigPage";
import { SmsSocketConfigPage } from "./SmsSocketConfigPage";
import { WhatsAppConfigPage } from "./WhatsAppConfigPage";

const KNOWN_CONFIGS: Record<string, PluginConfigComponent> = {
    // Each new plugin with a custom config UI registers here.
    // The component is wrapped by the shell below — it can't
    // remove the danger zone or back button.
    "google-contacts": GoogleContactsPage,
    "google-calendar": GoogleCalendarPage,
    "google-places": GooglePlacesConfigPage,
    pushover: PushoverConfigPage,
    signal: SignalConfigPage,
    slack: SlackConfigPage,
    "sms-socket": SmsSocketConfigPage,
    whatsapp: WhatsAppConfigPage,
};

export function PluginConfigRouter() {
    const { plugin_id } = useParams<{ plugin_id: string }>();
    const navigate = useNavigate();
    const { getAccessToken } = useAuth();
    const id = plugin_id ?? "";
    // Don't fall through to the Component when id is empty or
    // the literal string "undefined" — both are misrouting
    // signals (URL was constructed from an undefined value
    // upstream) and would cause the inner component to fire
    // /api/admin/oauth/clients/undefined/controller requests.
    const idLooksValid = id.length > 0 && id !== "undefined" && id !== "null";
    const Component = idLooksValid ? KNOWN_CONFIGS[id] : undefined;

    const [summary, setSummary] = useState<PluginSummary | "loading" | null>(
        "loading",
    );
    const [error, setError] = useState<string | null>(null);
    const [uninstalling, setUninstalling] = useState(false);
    const [resetting, setResetting] = useState(false);
    /// Result of the most recent factory-reset call, used to render
    /// a transient success message ("Wiped 1 sidecar volume + 2
    /// vault entries"). Cleared on next refresh / next reset.
    const [resetResult, setResetResult] =
        useState<FactoryResetPluginResponse | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listPlugins(getAccessToken);
            const found = r.plugins.find((p) => p.plugin_id === id);
            setSummary(found ?? null);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setSummary(null);
        }
    }, [getAccessToken, id]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onUninstall = useCallback(async () => {
        if (
            !window.confirm(
                `Uninstall plugin "${id}"? This is permanent — the staged plugin files are removed and any plugin-specific OAuth tokens / config are dropped.`,
            )
        ) {
            return;
        }
        setUninstalling(true);
        try {
            await uninstallPlugin(id, getAccessToken);
            navigate("/settings/plugins", { replace: true });
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setUninstalling(false);
        }
    }, [getAccessToken, id, navigate]);

    /// Wipe runtime state for this plugin and re-bootstrap. Distinct
    /// from uninstall: install record stays, plugin remains enabled,
    /// operator-supplied OAuth client credentials are preserved.
    /// Wired to `POST /api/admin/plugins/:id/factory-reset`.
    const onFactoryReset = useCallback(async () => {
        if (
            !window.confirm(
                `Factory-reset plugin "${id}"?\n\nThis wipes:\n  • cached vault entries (tokens, secrets the plugin stored)\n  • OAuth access + refresh tokens (re-authorize after)\n  • sidecar state volumes (signal-cli / wuzapi pairing)\n\nIt KEEPS:\n  • the plugin install itself (no need to reinstall)\n  • OAuth client credentials you typed in (client_id, client_secret)\n\nThe plugin is disabled, wiped, then re-enabled. Existing inbound conversations on bridged transports will need re-pairing.`,
            )
        ) {
            return;
        }
        setResetting(true);
        setResetResult(null);
        try {
            const r = await factoryResetPlugin(id, getAccessToken);
            setResetResult(r);
            setError(null);
            // Refresh the install summary so any state-derived UI
            // upstream (e.g. enabled chip) re-fetches; the plugin's
            // own config component will refetch its status on its
            // next poll cadence.
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setResetting(false);
        }
    }, [getAccessToken, id, refresh]);

    return (
        <section
            className="execlaw-settings__section"
            data-testid="plugin-config-shell"
        >
            <div className="d-flex align-items-center gap-2 mb-3">
                <Link
                    to="/settings/plugins"
                    className="btn btn-sm btn-outline-secondary"
                    data-testid="plugin-config-back"
                >
                    <i className="bi bi-arrow-left me-1" aria-hidden />
                    Plugins
                </Link>
                <h3 className="h6 mb-0 ms-1">{id}</h3>
                {summary && summary !== "loading" && (
                    <>
                        <span className="execlaw-muted small ms-2">
                            v{summary.version}
                        </span>
                        <span
                            className={
                                "execlaw-trust-badge ms-1" +
                                (summary.enabled ? " is-known" : "")
                            }
                        >
                            {summary.enabled ? "enabled" : "disabled"}
                        </span>
                    </>
                )}
            </div>

            {/* Plugin-supplied content. */}
            {!idLooksValid ? (
                <div className="execlaw-card border border-warning-subtle">
                    <div className="execlaw-card__title text-warning">
                        <i className="bi bi-exclamation-triangle me-2" aria-hidden />
                        Invalid plugin id in URL
                    </div>
                    <div className="execlaw-muted small">
                        The route <code>/settings/plugins/{id || "(empty)"}</code>{" "}
                        is malformed — likely a stale link generated from an
                        undefined value. Go back to{" "}
                        <Link to="/settings/plugins">Plugins</Link> and try
                        again.
                    </div>
                </div>
            ) : Component ? (
                <Component
                    pluginId={id}
                    pluginVersion={
                        summary && summary !== "loading" ? summary.version : ""
                    }
                    onConfigChanged={refresh}
                />
            ) : (
                <div className="execlaw-card">
                    <div className="execlaw-card__title">
                        <i className="bi bi-info-circle me-2" aria-hidden />
                        No configuration UI available
                    </div>
                    <div className="execlaw-muted small">
                        The plugin <code>{id}</code> doesn't ship a custom
                        configuration form. Future manifest-declared{" "}
                        <code>[[settings_fields]]</code> will populate this
                        view automatically.
                    </div>
                </div>
            )}

            {/* Danger zone — owned by the shell, always rendered. The
                plugin component above CANNOT remove this. */}
            <div
                className="execlaw-card mt-4 border border-danger-subtle"
                data-testid="plugin-config-danger-zone"
            >
                <div className="execlaw-card__title text-danger">
                    <i className="bi bi-exclamation-triangle me-2" aria-hidden />
                    Danger zone
                </div>

                <ErrorBanner
                    message={error}
                    onDismiss={() => setError(null)}
                    className="mb-2"
                />

                {resetResult && (
                    <div
                        className="alert alert-success small py-2 mb-3"
                        data-testid="plugin-config-reset-result"
                    >
                        Factory reset complete:{" "}
                        <strong>{resetResult.sidecars_wiped}</strong> sidecar
                        volume(s),{" "}
                        <strong>{resetResult.vault_keys_wiped}</strong> vault
                        entr{resetResult.vault_keys_wiped === 1 ? "y" : "ies"},
                        {" "}
                        <strong>{resetResult.oauth_tokens_wiped}</strong> OAuth
                        token(s) wiped. Plugin re-enabled.
                    </div>
                )}

                {/* Factory reset — non-destructive to the install itself.
                    Wipes runtime state (vault, sidecar volumes, OAuth
                    tokens) but keeps the install record and any operator-
                    typed config. Pairs with the existing Uninstall button
                    below for the case where wiping isn't enough. */}
                <div className="execlaw-muted small mb-2">
                    <strong>Factory reset</strong> wipes the plugin's runtime
                    state (cached vault entries, OAuth tokens, sidecar volumes
                    such as signal-cli / wuzapi pairing) but keeps the install
                    record and your operator-typed config (e.g. OAuth client
                    credentials). Use this when a sidecar is in a bad state
                    or pairing has drifted.
                </div>
                <Button
                    variant="outline-warning"
                    size="sm"
                    className="mb-3"
                    onClick={() => void onFactoryReset()}
                    disabled={resetting || uninstalling || summary === "loading"}
                    data-testid="plugin-config-factory-reset"
                >
                    {resetting ? (
                        <>
                            <Spinner size="sm" animation="border" className="me-1" />
                            Resetting…
                        </>
                    ) : (
                        <>
                            <i className="bi bi-arrow-counterclockwise me-1" aria-hidden />
                            Factory reset {id}
                        </>
                    )}
                </Button>

                <hr className="my-3" />

                <div className="execlaw-muted small mb-2">
                    <strong>Uninstall</strong> removes the staged plugin files
                    plus any plugin-scoped credentials in the vault. The hooks
                    the plugin registered (tools, identity providers, etc.)
                    are dropped immediately.
                </div>
                <Button
                    variant="outline-danger"
                    size="sm"
                    onClick={() => void onUninstall()}
                    disabled={uninstalling || resetting || summary === "loading"}
                    data-testid="plugin-config-uninstall"
                >
                    {uninstalling ? (
                        <>
                            <Spinner size="sm" animation="border" className="me-1" />
                            Uninstalling…
                        </>
                    ) : (
                        <>
                            <i className="bi bi-trash3 me-1" aria-hidden />
                            Uninstall {id}
                        </>
                    )}
                </Button>
            </div>
        </section>
    );
}
