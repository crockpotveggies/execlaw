/**
 * Public contract for plugin-authored UI panels.
 *
 * Every plugin that declares `[[ui_panels]]` in its manifest ships a
 * `ui/panel.tsx` (or `.ts`) whose default export is a React function
 * component conforming to `PluginPanelComponent` defined below. The
 * host loads that bundle dynamically at runtime — see
 * `web/src/settings/DynamicPluginPanel.tsx` — and renders the
 * component INSIDE a host-owned scaffold (`PluginScaffold.tsx`)
 * which is responsible for the page chrome (title, breadcrumbs,
 * **Danger Zone with Uninstall button**, error boundary). Plugin
 * authors cannot override the scaffold; that's the load-bearing
 * invariant — operators must always see the same "Uninstall"
 * affordance for every plugin, even broken ones.
 *
 * # Authoring a plugin panel
 *
 * 1. Create `plugins/<your-plugin>/ui/panel.tsx`.
 * 2. Import types from this module (type-only — no runtime
 *    artifacts ship from the host into your bundle).
 * 3. Use the bridge to access React, helpers, and shared UI
 *    components — see `BridgeApi` below. Do NOT import React or
 *    any host module directly; you'd ship duplicate copies and
 *    React's "Invalid hook call" guard would tear your panel down.
 * 4. Build via the shared `scripts/build-plugin-ui.sh <name>` step.
 *    Result: `plugins/<name>/ui/panel.js`, marked external for
 *    `react`, `react-dom`, `@execlaw/plugin-ui` — the bundler
 *    rewrites those imports to `globalThis.execlawHost.*`.
 *
 * # Why a typed interface (not a base class)
 *
 * Function components are how modern React is authored, so the
 * "extends a base component" you asked for is realised as
 * `extends PluginPanelComponent` at the type level — your panel
 * is a function with this signature. The scaffold-wraps-plugin
 * relationship gives the same containment guarantee a class
 * hierarchy would, without forcing class syntax on plugin authors.
 *
 * @module @execlaw/plugin-ui
 */

import type { ReactNode } from "react";

/**
 * Identity + scoping context the host passes to every plugin panel.
 *
 * Plugins use these values to address their own admin routes
 * (`/api/admin/plugins/{pluginId}/...`) and to display the operator-
 * friendly plugin name without having to read the manifest a second
 * time. The host fills these in from the manifest at mount time;
 * plugins must NOT mutate them.
 */
export interface PluginIdentity {
    /** The plugin's `[plugin].id` — stable, kebab-case (e.g. "signal"). */
    readonly id: string;
    /** The plugin's `[plugin].name` — operator-facing label. */
    readonly displayName: string;
    /** The plugin's `[plugin].version` — semver string. */
    readonly version: string;
}

/**
 * Props the host passes to a plugin's panel component on mount.
 *
 * Kept intentionally small so the API surface stays stable across
 * host releases. Plugins that need additional state should read it
 * from their own admin routes via `bridge.fetchJson`.
 */
export interface PluginPanelProps {
    /** Who the plugin is — same identity the manifest declares. */
    readonly identity: PluginIdentity;
    /** Shared host services. See `BridgeApi`. */
    readonly bridge: BridgeApi;
}

/**
 * The function signature every plugin panel's default export must
 * conform to.
 *
 * @example
 * ```tsx
 * // plugins/signal/ui/panel.tsx
 * import type { PluginPanelComponent } from "@execlaw/plugin-ui";
 * const Panel: PluginPanelComponent = ({ identity, bridge }) => {
 *     const { React } = bridge;
 *     const [status, setStatus] = React.useState(null);
 *     // ...
 *     return <div>Signal config for {identity.displayName}</div>;
 * };
 * export default Panel;
 * ```
 */
export type PluginPanelComponent = (props: PluginPanelProps) => ReactNode;

/**
 * The bridge API the host exposes to every plugin panel.
 *
 * Plugins access this object via `props.bridge` rather than via a
 * direct `import`. Behind the scenes the bridge instance is the
 * same `window.execlawHost` global the host puts on the page at
 * boot — passing it via props keeps the plugin's component pure
 * (testable with a mock bridge) while still avoiding the
 * duplicate-React hazard.
 */
export interface BridgeApi {
    /**
     * The host's React module. **You MUST use this React** — not
     * a copy bundled into your plugin. Two React copies on a page
     * = "Invalid hook call" runtime crash.
     */
    readonly React: typeof import("react");
    /**
     * The host's ReactDOM (for portals, `flushSync`, etc.). Same
     * "single copy" rule applies.
     */
    readonly ReactDOM: typeof import("react-dom");

    /**
     * Return the current operator's access token, or `null` if the
     * SPA is logged-out / between sessions. Synchronous: the auth
     * context keeps the latest token in a ref so this is a cheap
     * read with no I/O. Pass into `bridge.fetchJson` (which already
     * does this for you) or any direct `fetch()` that needs the
     * bearer header.
     *
     * Token refresh happens in the AuthContext's background — by
     * the time a plugin panel reads the token here, it's the
     * latest valid one. If `null` is returned the operator was
     * signed out mid-flight; surface an error rather than
     * silently sending an unauthorised request.
     */
    getAccessToken(): string | null;

    /**
     * Authenticated JSON fetch helper. Threads the access token
     * into the `Authorization` header, parses JSON, and rejects
     * on non-2xx with an `Error` whose `.message` carries the
     * response body. Use this for every host API call from inside
     * a plugin panel.
     *
     * @param method HTTP verb (default `"GET"`).
     * @param path   API path under the host. `/api/admin/plugins/{id}/...`
     *               for plugin-scoped admin endpoints.
     * @param body   Optional JSON-serialisable body.
     */
    fetchJson<T = unknown>(
        method: string,
        path: string,
        body?: unknown,
    ): Promise<T>;

    /**
     * Helper for the very common pattern: fetch a status, poll it
     * on an interval while the page is open. Returns a tuple of
     * the latest value + the last error (if any). Cleanup happens
     * automatically when the consuming component unmounts.
     */
    usePoll<T>(
        fetcher: () => Promise<T>,
        intervalMs: number,
    ): { value: T | null; error: string | null };

    /**
     * Shared UI components the host exposes for visual consistency
     * across plugins. Plugins are NOT required to use these — you
     * can render any JSX — but reaching for these means your panel
     * matches the host's chrome out of the box.
     */
    readonly components: BridgeComponents;
}

/**
 * Shared React components plugins can use to match the host's
 * visual language without re-implementing chrome.
 */
export interface BridgeComponents {
    /** Dismissable red banner for surfacing API errors at the top of a panel. */
    readonly ErrorBanner: PluginComponent<ErrorBannerProps>;
    /** Sidecar health chip — used by Signal / WhatsApp config pages. */
    readonly SidecarStatusBlock: PluginComponent<SidecarStatusBlockProps>;
    /** Bootstrap-styled button. Pass through standard HTML button props. */
    readonly Button: PluginComponent<ButtonProps>;
}

/**
 * Function-component shape with explicit `displayName` so the
 * React devtools surface a useful label for components ferried
 * across the bridge.
 */
export type PluginComponent<P> = ((props: P) => ReactNode) & {
    displayName?: string;
};

// --- ErrorBanner -----------------------------------------------------

export interface ErrorBannerProps {
    /** Error text to display. `null` = banner hidden. */
    message: string | null;
    /** Called when the operator clicks the close affordance. */
    onDismiss: () => void;
    /** Optional Bootstrap class string for vertical spacing etc. */
    className?: string;
}

// --- SidecarStatusBlock ---------------------------------------------

export interface SidecarStatusBlockProps {
    /** Display label — typically the manifest's `[[services]].name`. */
    sidecarLabel: string;
    /** Live status string from `/api/admin/plugins/.../status`. */
    status: string;
    /** Loopback RPC URL once the sidecar has spawned; `null` otherwise. */
    rpcUrl: string | null;
    /** Last error from the host's RPC probe, if any. */
    fetchError?: string | null;
    /** Prefix for `data-testid` attributes inside the block. */
    testidPrefix: string;
}

// --- Button ----------------------------------------------------------

export interface ButtonProps {
    children: ReactNode;
    onClick?: (event: unknown) => void;
    disabled?: boolean;
    variant?:
        | "primary"
        | "secondary"
        | "danger"
        | "outline-primary"
        | "outline-secondary"
        | "outline-danger";
    size?: "sm" | "lg";
    type?: "button" | "submit" | "reset";
    className?: string;
    "data-testid"?: string;
}

// --- globalThis declaration ----------------------------------------

declare global {
    /**
     * The host bridge instance, installed by the SPA at boot. Plugin
     * panels access this indirectly via `props.bridge` — direct use
     * of the global is reserved for the bundle's hot-path
     * externalisation rewriter.
     */
    // eslint-disable-next-line no-var
    var execlawHost: BridgeApi | undefined;
}

export {};
