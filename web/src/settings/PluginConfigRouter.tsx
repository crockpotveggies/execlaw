// Per-plugin config page shell. URL: `/settings/plugins/:plugin_id`.
//
// Architecture (post-2026-05-14 self-containment refactor):
//
//   PluginConfigShell (THIS file — owns the chrome: header + danger zone)
//     ├─ <DynamicPluginPanel ... />
//     │     └─ loads the plugin's own ui/panel.js at runtime
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
// All built-in plugins were migrated to the self-contained
// `ui/panel.tsx` shape between 2026-05-14 and 2026-05-15
// (signal, then the remaining ten). The `STATIC_FALLBACKS` map
// below is now empty; it's kept as the documented escape hatch
// for any future plugin whose config UI hasn't been packaged
// alongside its main.rhai yet.

import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";
import {
    getSetupPreflight,
    listPlugins,
    uninstallPlugin,
    type PluginSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { DynamicPluginPanel } from "./DynamicPluginPanel";
import type { PluginConfigComponent } from "./PluginConfigBase";

/**
 * Transitional fallback map. Each entry is a plugin whose UI has
 * NOT yet been migrated into its own ZIP. The shell renders the
 * fallback only when the dynamic load (from the plugin's
 * `ui/panel.js`) 404s — i.e. when the plugin doesn't ship one yet.
 * Empty as of 2026-05-15: every built-in plugin now ships its own
 * panel.js. Add an entry here only if onboarding a new plugin
 * whose UI hasn't been packaged yet.
 */
const STATIC_FALLBACKS: Record<string, PluginConfigComponent> = {};

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
    // Docker availability — fetched lazily ONLY when the loaded
    // plugin declares sidecars (manifest has `[[services]]`). On
    // Apple-Silicon installs the setup wizard auto-skips the
    // Docker step, so the operator finishes setup with no Docker
    // check; if they then install a plugin like python-sandbox
    // that needs Docker, the sidecar would silently fail to spawn
    // with no SPA signal. The banner below makes the mismatch
    // explicit. `null` = not yet probed; `true` / `false` are the
    // resolved states.
    const [dockerAvailable, setDockerAvailable] = useState<boolean | null>(null);

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

    // Probe Docker only when we know the plugin needs it. Older
    // server builds may omit `has_sidecars`; treat undefined as
    // false so we don't spam preflight for plugins that don't
    // need the check.
    useEffect(() => {
        if (
            summary === "loading" ||
            summary === null ||
            !summary.has_sidecars
        ) {
            return;
        }
        let cancelled = false;
        void (async () => {
            try {
                const p = await getSetupPreflight(getAccessToken);
                if (!cancelled) {
                    setDockerAvailable(p.docker.available === true);
                }
            } catch {
                // Preflight failure isn't fatal — leave the banner
                // off and let the operator find out via the
                // sidecar status block instead. Better than a
                // spurious "Docker missing" claim when the real
                // failure is /api/admin/setup/preflight itself.
                if (!cancelled) {
                    setDockerAvailable(null);
                }
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [summary, getAccessToken]);

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

            {/* Docker-required warning. Renders only when we
                confirmed (a) the plugin's manifest declares
                sidecars AND (b) preflight reported Docker
                unavailable. The wizard auto-skips the Docker step
                on Apple Silicon (Ollama runs natively, no
                Metal-in-container path), so an operator who later
                installs a sidecar-bearing plugin would otherwise
                get no signal. */}
            {summary &&
                summary !== "loading" &&
                summary.has_sidecars === true &&
                dockerAvailable === false && (
                    <div
                        className="execlaw-card mb-3 border border-warning-subtle"
                        data-testid="plugin-config-docker-missing"
                    >
                        <div className="execlaw-card__title text-warning d-flex align-items-center">
                            <i
                                className="bi bi-exclamation-triangle-fill me-2"
                                aria-hidden
                            />
                            Docker is required for this plugin
                        </div>
                        <div className="execlaw-muted small mb-2">
                            <strong>{id}</strong> declares one or more sidecar
                            containers in its manifest. execlaw spawns those
                            sidecars via Docker, but the Docker daemon is not
                            reachable on this host. The plugin&rsquo;s panel
                            below will load, but any feature that depends on
                            the sidecar will fail until Docker is running.
                        </div>
                        <div className="execlaw-muted small">
                            Apple Silicon note: the setup wizard skips Docker
                            because the inference path uses Ollama natively.
                            Sidecar plugins still need Docker Desktop or an
                            equivalent runtime — install it from{" "}
                            <a
                                href="https://www.docker.com/products/docker-desktop/"
                                target="_blank"
                                rel="noreferrer noopener"
                            >
                                docker.com
                            </a>{" "}
                            and restart execlaw to pick it up.
                        </div>
                    </div>
                )}

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
