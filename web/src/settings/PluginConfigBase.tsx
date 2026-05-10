// Contract every per-plugin configuration component must satisfy.
//
// Plugin authors write a `(props: PluginConfigProps) => JSX.Element`
// — they get the plugin's id + version + a host-provided refresh
// callback, and they render whatever middle-of-the-page content
// they want. They DO NOT get to render their own header or
// danger-zone footer; those are owned by `PluginConfigShell`
// (uninstall is intentionally not overridable).

import type { JSX } from "react";

export interface PluginConfigProps {
    /// Plugin id from the manifest. Plugins use this to scope
    /// API calls (e.g. `/api/admin/oauth/clients/<id>/...`).
    pluginId: string;
    /// Plugin version string from the manifest. Surfaced in the
    /// shell's header; plugins receive it for context (e.g. for
    /// version-specific feature flags).
    pluginVersion: string;
    /// Called by the plugin when something it persisted (OAuth
    /// connect, settings save, etc.) might have changed the
    /// summary the shell renders. The shell currently re-fetches
    /// `/api/admin/plugins` so the row's enable badge / version
    /// stay in sync. Optional — many plugins won't need it.
    onConfigChanged?: () => void;
}

/// Type alias for the function-component shape we accept in the
/// `PluginConfigRouter` switch. New plugin config components
/// must satisfy this signature.
export type PluginConfigComponent = (props: PluginConfigProps) => JSX.Element;
