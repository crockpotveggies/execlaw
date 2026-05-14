// Google Contacts plugin self-contained config panel.
//
// Migrated from `web/src/settings/GoogleContactsPage.tsx` (2026-05-14).
// Delegates to the local `OauthClientConfig` (sibling file) for the
// shared OAuth-client + Connect/Disconnect machinery.
//
// Build: node scripts/build-plugin-ui.mjs google-contacts

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
void React; // module-scope React const used by the JSX factory below.

import { OauthClientConfig } from "./oauth-client-config";

const SCOPES = [
    "https://www.googleapis.com/auth/contacts.readonly",
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
            title="Google Contacts"
            icon="bi-person-vcard"
            description={
                <>
                    Connects the <code>{identity.id}</code> plugin to your Google
                    account. Saved contacts auto-trust as Contact-class
                    principals; the <code>contacts.list</code> tool becomes
                    available on chat turns.
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
                            href="https://console.cloud.google.com/apis/library/people.googleapis.com"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            People API
                        </a>{" "}
                        for the project.
                    </li>
                    <li>
                        Create an OAuth 2.0 Client ID of type{" "}
                        <strong>Web application</strong>.
                    </li>
                    <li>
                        Under Authorized redirect URIs, add this server&apos;s
                        callback URL (shown in the form above).
                    </li>
                    <li>
                        Paste the resulting client ID + secret above and
                        click Save, then Connect Account.
                    </li>
                </ol>
            }
        />
    );
};

export default Panel;
