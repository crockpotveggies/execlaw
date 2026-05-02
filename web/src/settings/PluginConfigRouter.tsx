// Per-plugin config page router. URL: `/settings/plugins/:plugin_id`.
//
// Today: hardcoded mapping from plugin_id → config component.
// Each new plugin with a settings UI registers itself here.
//
// Tomorrow (when the schema-driven `[[settings_fields]]` mechanism
// lands): this fallback case becomes a generic form renderer that
// reads the plugin's manifest field declarations + the existing
// OAuth admin endpoints, so plugin authors don't have to touch
// the SPA at all. Until then, plugins that need a custom shape
// (anything beyond a flat OAuth + form) drop a sibling component
// here and register it in the switch below.

import { Link, useParams } from "react-router-dom";
import { GoogleContactsPage } from "./GoogleContactsPage";

const KNOWN_CONFIGS: Record<string, () => JSX.Element> = {
    "google-contacts": () => <GoogleContactsPage />,
};

export function PluginConfigRouter() {
    const { plugin_id } = useParams<{ plugin_id: string }>();
    const id = plugin_id ?? "";
    const Component = KNOWN_CONFIGS[id];
    return (
        <section className="execlaw-settings__section" data-testid="plugin-config-router">
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
            </div>
            {Component ? (
                <Component />
            ) : (
                <div className="execlaw-card">
                    <div className="execlaw-card__title">
                        <i className="bi bi-info-circle me-2" aria-hidden />
                        No configuration UI available
                    </div>
                    <div className="execlaw-muted small">
                        The plugin <code>{id}</code> doesn't ship a
                        configuration form, or this build of execlaw
                        doesn't have one registered for it. Future
                        manifest-declared <code>[[settings_fields]]</code>
                        will populate this view automatically.
                    </div>
                </div>
            )}
        </section>
    );
}
