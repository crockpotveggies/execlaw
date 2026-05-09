// Settings → Google Contacts plugin config (Phase 9, plugin-google-contacts).
//
// Mounted by `PluginConfigRouter` inside `PluginConfigShell`,
// which owns the page header (back button + title + version
// badge) and the danger-zone footer (Uninstall) — this component
// renders ONLY the middle slot.
//
// All the heavy lifting (OAuth client form, Connect/Disconnect,
// state machine) lives in the shared `OauthClientConfig`
// component. This file just supplies the plugin-specific copy.

import type { PluginConfigProps } from "./PluginConfigBase";
import { OauthClientConfig } from "./OauthClientConfig";

const SCOPES = [
    "https://www.googleapis.com/auth/contacts.readonly",
    "openid",
    "email",
];

export function GoogleContactsPage({
    pluginId,
    onConfigChanged,
}: PluginConfigProps) {
    return (
        <OauthClientConfig
            pluginId={pluginId}
            provider="google"
            defaultScopes={SCOPES}
            title="Google Contacts"
            icon="bi-person-vcard"
            description={
                <>
                    Connects the <code>{pluginId}</code> plugin to your Google
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
                        Under Authorized redirect URIs, add this server's
                        callback URL (shown in the form above).
                    </li>
                    <li>
                        Paste the resulting client ID + secret above and
                        click Save, then Connect Account.
                    </li>
                </ol>
            }
            onConfigChanged={onConfigChanged}
        />
    );
}
