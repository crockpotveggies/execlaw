// Per-plugin config page shell. URL: `/settings/plugins/:plugin_id`.
//
// Architecture (post-2026-05-14 self-containment refactor):
//
//   PluginConfigShell (THIS file — owns the chrome: header + danger zone)
//     ├─ <DynamicPluginPanel ... staticFallback={KNOWN_CONFIGS[id]} />
//     │     └─ tries plugin's own ui/panel.js first; falls back to
//     │        any hardcoded SPA page that hasn't migrated yet
//     └─ DangerZone (Uninstall) — unconditional, plugin can't override
//
// Plugin authors ship `ui/panel.tsx` inside their ZIP — see
// `web/src/plugins/types.ts` for the `PluginPanelComponent`
// contract + `docs/plugins.md` for the authoring walkthrough.
// The host's DynamicPluginPanel loads the plugin's `ui/panel.js`
// at runtime (via authenticated fetch + Blob URL +
// `import()`), passes the bridge (`globalThis.execlawHost`), and
// renders the result.
//
// Plugin authors cannot skip the Danger Zone — it's rendered
// outside the plugin's panel in the shell below. Same goes for
// the header (back button, plugin id, version, enabled badge).
// That's the load-bearing containment invariant: every plugin
// gets the same lifecycle UI, even if its own panel crashes.
//
// `KNOWN_CONFIGS` is a transitional fallback for plugins that
// haven't yet migrated their config UI into the plugin ZIP. Each
// entry is removed as the corresponding plugin is migrated; the
// final state is an empty record (or the var deleted entirely).

import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";
import {
    listPlugins,
    uninstallPlugin,
    type PluginSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { DiscordConfigPage } from "./DiscordConfigPage";
import { DynamicPluginPanel } from "./DynamicPluginPanel";
import { GoogleAppsPage } from "./GoogleAppsPage";
import { GoogleCalendarPage } from "./GoogleCalendarPage";
import { GoogleContactsPage } from "./GoogleContactsPage";
import { GooglePlacesConfigPage } from "./GooglePlacesConfigPage";
import { OpenMeteoConfigPage } from "./OpenMeteoConfigPage";
import type { PluginConfigComponent } from "./PluginConfigBase";
import { PushoverConfigPage } from "./PushoverConfigPage";
// SignalConfigPage retired 2026-05-14 — Signal now ships its own
// ui/panel.js inside the plugin ZIP (plugin v0.5.0+). The dynamic
// loader resolves it via /api/admin/plugins/signal/ui/panel.js.
import { SlackConfigPage } from "./SlackConfigPage";
import { SmsSocketConfigPage } from "./SmsSocketConfigPage";
import { WhatsAppConfigPage } from "./WhatsAppConfigPage";

/**
 * Transitional fallback map. Each entry is a plugin whose UI has
 * NOT yet been migrated into its own ZIP. The shell renders the
 * fallback only when the dynamic load (from the plugin's
 * `ui/panel.js`) 404s — i.e. when the plugin doesn't ship one yet.
 * As each plugin is migrated, its entry is removed; the final
 * state of this map is `{}` and the whole import block goes away.
 */
const STATIC_FALLBACKS: Record<string, PluginConfigComponent> = {
    discord: DiscordConfigPage,
    "google-apps": GoogleAppsPage,
    "google-contacts": GoogleContactsPage,
    "google-calendar": GoogleCalendarPage,
    "google-places": GooglePlacesConfigPage,
    "open-meteo": OpenMeteoConfigPage,
    pushover: PushoverConfigPage,
    // signal: migrated 2026-05-14 — see plugins/signal/ui/panel.tsx.
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
    const StaticFallback = idLooksValid ? STATIC_FALLBACKS[id] : undefined;

    const [summary, setSummary] = useState<PluginSummary | "loading" | null>(
        "loading",
    );
    const [error, setError] = useState<string | null>(null);
    const [uninstalling, setUninstalling] = useState(false);

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
            ) : (
                <DynamicPluginPanel
                    pluginId={id}
                    pluginVersion={
                        summary && summary !== "loading"
                            ? summary.version
                            : ""
                    }
                    pluginDisplayName={id}
                    onConfigChanged={refresh}
                    staticFallback={
                        StaticFallback
                            ? () => (
                                  <StaticFallback
                                      pluginId={id}
                                      pluginVersion={
                                          summary && summary !== "loading"
                                              ? summary.version
                                              : ""
                                      }
                                      onConfigChanged={refresh}
                                  />
                              )
                            : undefined
                    }
                />
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
                <div className="execlaw-muted small mb-2">
                    Uninstalling removes the staged plugin files plus any
                    plugin-scoped credentials in the vault. The hooks the
                    plugin registered (tools, identity providers, etc.)
                    are dropped immediately.
                </div>
                <ErrorBanner
                    message={error}
                    onDismiss={() => setError(null)}
                    className="mb-2"
                />
                <Button
                    variant="outline-danger"
                    size="sm"
                    onClick={() => void onUninstall()}
                    disabled={uninstalling || summary === "loading"}
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
