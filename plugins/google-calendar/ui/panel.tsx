// Google Calendar plugin self-contained config panel.
//
// Migrated from `web/src/settings/GoogleCalendarPage.tsx` (2026-05-14).
// Delegates to the local `OauthClientConfig` (sibling file) for the
// shared OAuth-client + Connect/Disconnect machinery.
//
// Build: node scripts/build-plugin-ui.mjs google-calendar

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
void React; // satisfy `verbatimModuleSyntax` — React is used by the JSX factory below.

import { OauthClientConfig } from "./oauth-client-config";

const SCOPES = [
    "https://www.googleapis.com/auth/calendar.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "openid",
    "email",
];

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { identity, bridge } = props;
    return (
        <OauthClientConfig
            pluginId={identity.id}
            bridge={bridge}
            provider="google"
            defaultScopes={SCOPES}
            title="Google Calendar"
            icon="bi-calendar3"
            description={
                <>
                    Connects the <code>{identity.id}</code> plugin to your Google
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
                        Under Authorized redirect URIs, add this server&apos;s
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
        />
    );
};

export default Panel;
