// Settings → Google Calendar plugin config (Phase 9, plugin-google-calendar).
//
// Same shape as GoogleContactsPage — both delegate to the shared
// OauthClientConfig component. Plugin-specific differences:
// scopes, title/icon, description copy, Cloud Console setup
// steps (Calendar API, not People API).

import type { PluginConfigProps } from "./PluginConfigBase";
import { OauthClientConfig } from "./OauthClientConfig";

const SCOPES = [
    "https://www.googleapis.com/auth/calendar.readonly",
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
                    account. The <code>calendar.list_calendars</code> and{" "}
                    <code>calendar.list_events</code> tools become available
                    on chat turns. Read-only — write operations land when the
                    operator-confirm flow for outbound calendar mutations is
                    wired.
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
                        will request the{" "}
                        <code>calendar.readonly</code> scope.
                    </li>
                </ol>
            }
            onConfigChanged={onConfigChanged}
        />
    );
}
