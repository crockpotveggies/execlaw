/**
 * Loads a plugin's self-contained UI panel at runtime and mounts it
 * inside the host-owned scaffold (`PluginConfigRouter`).
 *
 * The flow:
 *
 *   1. Authenticated `fetch()` to
 *      `GET /api/admin/plugins/{plugin_id}/ui/panel.js`. The
 *      server-side route (see `crates/server/src/plugin_admin_routes.rs`)
 *      streams the file from the plugin's staged ZIP directory.
 *      404 = the plugin doesn't ship a UI panel; we render the
 *      "no configuration UI" fallback. Any other 4xx/5xx is an
 *      error banner — operators can retry or open the network tab.
 *
 *   2. Wrap the JS text in a `URL.createObjectURL` blob and
 *      dynamic-`import()` it. Authenticated fetch + blob URL is
 *      the only way to ferry JWT-bearer-protected JS into the
 *      module loader; raw `import()` doesn't carry the
 *      `Authorization` header.
 *
 *   3. Call the module's default export with
 *      `{ identity, bridge: globalThis.execlawHost }` and render
 *      the returned React element.
 *
 * The plugin's bundle declares React + `@execlaw/plugin-ui` as
 * externals — the shared build script (`scripts/build-plugin-ui.sh`)
 * rewrites those imports into reads against `globalThis.execlawHost`,
 * so a plugin panel never ships its own React copy and the
 * "Invalid hook call" duplicate-React crash is structurally
 * impossible.
 *
 * If the plugin's module throws on load or its default export is
 * not a function, we surface a clear error inside the scaffold —
 * the operator still sees the Danger Zone / Uninstall affordance
 * and can recover.
 */

import { useCallback, useEffect, useState } from "react";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type {
    PluginIdentity,
    PluginPanelComponent,
    PluginPanelProps,
} from "../plugins/types";

interface Props {
    /** Plugin to load. Matches `state_plugins.plugin_id`. */
    pluginId: string;
    /** Version label rendered in the no-UI fallback. */
    pluginVersion: string;
    /** Operator-facing label rendered as the panel's identity name. */
    pluginDisplayName: string;
    /** Refresh callback the panel can invoke after config changes. */
    onConfigChanged: () => void;
    /**
     * Transitional fallback for plugins that haven't yet shipped a
     * `ui/panel.js` in their ZIP. When the dynamic load 404s AND
     * a fallback is provided, the fallback renders instead of the
     * "no configuration UI" card. Remove the fallback once the
     * plugin is migrated.
     *
     * The fallback is a render-prop (not a React element) so it
     * doesn't mount its tree unless we actually need it — avoids
     * the "loading then a flash of the fallback then the real
     * panel" hiccup during the dynamic-load probe.
     */
    staticFallback?: () => ReactNode;
}

/**
 * Internal state for the dynamic load: either we're loading, we've
 * resolved a component, or we're in one of the documented error
 * states. We don't try to recover automatically — operators get a
 * "Retry" button so they can decide.
 */
type LoadState =
    | { kind: "loading" }
    | { kind: "no-panel" }
    | { kind: "ready"; Panel: PluginPanelComponent }
    | { kind: "error"; message: string };

export function DynamicPluginPanel({
    pluginId,
    pluginVersion,
    pluginDisplayName,
    onConfigChanged: _onConfigChanged,
    staticFallback,
}: Props) {
    const { getAccessToken, status: authStatus } = useAuth();
    const [state, setState] = useState<LoadState>({ kind: "loading" });
    const [reloadKey, setReloadKey] = useState(0);

    const triggerRetry = useCallback(() => setReloadKey((n) => n + 1), []);

    useEffect(() => {
        let cancelled = false;
        let blobUrl: string | null = null;

        // 2026-05-16 — wait for the AuthProvider to settle before
        // deciding whether the token is missing. Pre-fix, this
        // useEffect ran on first mount with auth still in `loading`
        // (the boot effect that hydrates accessTokenRef from
        // localStorage hadn't fired yet — React runs child effects
        // BEFORE parent effects), so `getAccessToken()` returned
        // null and the panel locked into "operator is signed out"
        // forever. Operator's only out was the manual Retry click.
        //
        // Now: while auth is still booting, stay in `loading`.
        // When auth settles to `unauthenticated`, surface the
        // signed-out error (the operator really is signed out).
        // When auth becomes `authenticated`, run the fetch.
        if (authStatus === "loading") {
            setState({ kind: "loading" });
            return;
        }
        setState({ kind: "loading" });

        (async () => {
            try {
                const token = getAccessToken();
                if (token === null) {
                    setState({
                        kind: "error",
                        message:
                            "access token unavailable — operator is signed out",
                    });
                    return;
                }
                const resp = await fetch(
                    `/api/admin/plugins/${encodeURIComponent(pluginId)}/ui/panel.js`,
                    {
                        method: "GET",
                        headers: { Authorization: `Bearer ${token}` },
                    },
                );
                if (cancelled) return;
                if (resp.status === 404) {
                    // Plugin doesn't ship a UI panel — fall through
                    // to the friendly "no configuration UI" card.
                    setState({ kind: "no-panel" });
                    return;
                }
                if (!resp.ok) {
                    const body = await resp.text();
                    setState({
                        kind: "error",
                        message: `panel.js fetch failed: ${resp.status} ${body}`,
                    });
                    return;
                }
                const source = await resp.text();
                if (cancelled) return;

                const blob = new Blob([source], {
                    type: "application/javascript",
                });
                blobUrl = URL.createObjectURL(blob);

                // Dynamic import. Vite picks this up at build time
                // and emits a runtime loader; with a Blob URL we
                // bypass Vite's static analysis (the URL is opaque)
                // and the browser's native module loader handles it.
                // The `/* @vite-ignore */` annotation tells Vite not
                // to try to analyse / pre-transform the import.
                const mod: { default?: unknown } = await import(
                    /* @vite-ignore */ blobUrl
                );
                if (cancelled) return;
                const Panel = mod.default;
                if (typeof Panel !== "function") {
                    setState({
                        kind: "error",
                        message:
                            "plugin's panel.js default export is not a function",
                    });
                    return;
                }
                setState({
                    kind: "ready",
                    Panel: Panel as PluginPanelComponent,
                });
            } catch (e) {
                if (cancelled) return;
                setState({
                    kind: "error",
                    message: e instanceof Error ? e.message : String(e),
                });
            }
        })();

        return () => {
            cancelled = true;
            if (blobUrl) {
                URL.revokeObjectURL(blobUrl);
            }
        };
    }, [getAccessToken, pluginId, reloadKey, authStatus]);

    if (state.kind === "loading") {
        return (
            <div
                className="execlaw-muted small"
                data-testid="plugin-panel-loading"
            >
                Loading plugin UI…
            </div>
        );
    }

    if (state.kind === "no-panel") {
        // Transitional path: if a hardcoded SPA-side fallback is
        // provided AND the plugin doesn't ship its own panel.js
        // yet, render the fallback so the operator sees a working
        // page during the cross-plugin migration. Each plugin
        // drops its fallback entry as it migrates.
        if (staticFallback) {
            return <>{staticFallback()}</>;
        }
        return (
            <div className="execlaw-card" data-testid="plugin-panel-no-ui">
                <div className="execlaw-card__title">
                    <i className="bi bi-info-circle me-2" aria-hidden />
                    No configuration UI available
                </div>
                <div className="execlaw-muted small">
                    The plugin <code>{pluginId}</code> doesn&apos;t ship a
                    custom configuration form. Future manifest-declared{" "}
                    <code>[[settings_fields]]</code> will populate this
                    view automatically.
                </div>
            </div>
        );
    }

    if (state.kind === "error") {
        return (
            <div data-testid="plugin-panel-error">
                <ErrorBanner
                    message={state.message}
                    onDismiss={triggerRetry}
                    dismissAfterMs={0}
                />
                <button
                    type="button"
                    className="btn btn-sm btn-outline-secondary"
                    onClick={triggerRetry}
                    data-testid="plugin-panel-retry"
                >
                    <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                    Retry
                </button>
            </div>
        );
    }

    // Ready — render the plugin's component inside its own boundary
    // so a render-time throw inside the plugin doesn't take down the
    // surrounding scaffold (Danger Zone in particular MUST stay
    // reachable even if a plugin crashes).
    const identity: PluginIdentity = {
        id: pluginId,
        displayName: pluginDisplayName,
        version: pluginVersion,
    };
    const bridge = globalThis.execlawHost;
    if (!bridge) {
        return (
            <ErrorBanner
                message="host bridge not installed — SPA boot order bug"
                onDismiss={triggerRetry}
                dismissAfterMs={0}
            />
        );
    }
    const props: PluginPanelProps = { identity, bridge };
    return (
        <PluginErrorBoundary pluginId={pluginId} onRetry={triggerRetry}>
            <state.Panel {...props} />
        </PluginErrorBoundary>
    );
}

// ---------------------------------------------------------------------
// PluginErrorBoundary — class component (React only supports error
// boundaries as classes). Catches render-time exceptions thrown
// inside the plugin's component tree and surfaces them WITHOUT
// unmounting the surrounding scaffold. The Danger Zone stays
// reachable even when the plugin's render crashes.
// ---------------------------------------------------------------------

import { Component, type ReactNode } from "react";

interface BoundaryProps {
    pluginId: string;
    onRetry: () => void;
    children: ReactNode;
}

interface BoundaryState {
    error: Error | null;
}

class PluginErrorBoundary extends Component<BoundaryProps, BoundaryState> {
    state: BoundaryState = { error: null };

    static getDerivedStateFromError(error: Error): BoundaryState {
        return { error };
    }

    componentDidCatch(error: Error, info: { componentStack?: string | null }) {
        // Log to the browser console so operators with devtools
        // open can grab the stack. Plugin crashes are useful to
        // surface for the plugin author, not just the operator.
        // eslint-disable-next-line no-console
        console.error(
            `[plugin:${this.props.pluginId}] panel crashed:`,
            error,
            info.componentStack ?? "",
        );
    }

    render() {
        if (this.state.error) {
            return (
                <div data-testid="plugin-panel-crash">
                    <ErrorBanner
                        message={`plugin '${this.props.pluginId}' crashed: ${this.state.error.message}`}
                        onDismiss={() => {
                            this.setState({ error: null });
                            this.props.onRetry();
                        }}
                        dismissAfterMs={0}
                    />
                    <button
                        type="button"
                        className="btn btn-sm btn-outline-secondary"
                        onClick={() => {
                            this.setState({ error: null });
                            this.props.onRetry();
                        }}
                        data-testid="plugin-panel-crash-retry"
                    >
                        <i
                            className="bi bi-arrow-clockwise me-1"
                            aria-hidden
                        />
                        Retry
                    </button>
                </div>
            );
        }
        return this.props.children;
    }
}
