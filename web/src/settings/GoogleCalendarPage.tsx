// Settings → Google Calendar plugin config (Phase 9, plugin-google-calendar).
//
// Same shape as GoogleContactsPage — both delegate to the shared
// OauthClientConfig component. Plugin-specific differences:
// scopes, title/icon, description copy, Cloud Console setup
// steps (Calendar API, not People API).

import type { PluginConfigProps } from "./PluginConfigBase";
import { OauthClientConfig } from "./OauthClientConfig";

// Mirror plugins/google-calendar/plugin.toml's [[oauth_accounts]]
// scopes. `calendar.events` is required for create/update/delete
// (the plugin lost write access in v0.1 because it shipped with
// `calendar.readonly` only). Keep this list in sync with the manifest;
// the connect handler also reconciles persisted scopes against the
// manifest as a defence in depth, but the SPA's Save still has to
// post the right list so the visible OAuth client config doesn't
// claim "read-only".
const SCOPES = [
    "https://www.googleapis.com/auth/calendar.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "openid",
    "email",
];

export function GoogleCalendarPage({
    pluginId,
    onConfigChanged,
}: PluginConfigProps) {
    return (
        <OauthClientConfig
            pluginId={pluginId}
            provider="google"
            defaultScopes={SCOPES}
            title="Google Calendar"
            icon="bi-calendar3"
            description={
                <>
                    Connects the <code>{pluginId}</code> plugin to your Google
                    account. Exposes seven calendar tools to the agent —{" "}
                    <code>list_calendars</code>, <code>list_events</code>,{" "}
                    <code>check_availability</code>, <code>get_event</code>,{" "}
                    <code>create_event</code>, <code>update_event</code>,{" "}
                    <code>delete_event</code>. Mutations send invitation /
                    update notifications when attendees are present.
                </>
            }
            setupSteps={
                <ol className="small mt-2">
                    <li>
                        Open the{" "}
                        <a
                            href="https://console.cloud.google.com/apis/credentials"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            Google Cloud Console → Credentials
                        </a>{" "}
                        page (create a project first if needed).
                    </li>
                    <li>
                        Enable the{" "}
                        <a
                            href="https://console.cloud.google.com/apis/library/calendar-json.googleapis.com"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            Google Calendar API
                        </a>{" "}
                        for the project.
                    </li>
                    <li>
                        Create an OAuth 2.0 Client ID of type{" "}
                        <strong>Web application</strong>. (If you already
                        have one for another Google plugin like
                        google-contacts, you can reuse it — Google scopes
                        accumulate per OAuth client.)
                    </li>
                    <li>
                        Under Authorized redirect URIs, add this server's
                        callback URL (shown in the form above).
                    </li>
                    <li>
                        Paste the resulting client ID + secret above and
                        click Save, then Connect Account. The consent screen
                        will request the <code>calendar.readonly</code> +{" "}
                        <code>calendar.events</code> scopes — the latter is
                        required for <code>create_event</code> /{" "}
                        <code>update_event</code> / <code>delete_event</code>.
                        If you previously connected on v0.1 (read-only), use
                        Disconnect first so Google re-prompts with the new
                        scope set.
                    </li>
                </ol>
            }
            onConfigChanged={onConfigChanged}
        />
    );
}
