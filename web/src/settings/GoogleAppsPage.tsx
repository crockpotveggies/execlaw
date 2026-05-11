// Settings → Google Apps plugin config.
//
// Consolidated Google plugin — Gmail + Calendar + Contacts + Tasks +
// Drive in a single OAuth grant. Operators turn individual modules
// on/off via toggles stored in
// `/api/admin/plugins/google-apps/settings/enabled_modules`.
//
// The page composes two pieces:
//
//   * `OauthClientConfig` — shared OAuth client + Connect/Disconnect
//     UI. Same shape as GoogleCalendarPage / GoogleContactsPage but
//     with the union scope list.
//   * `ModuleToggles` — checkbox row for the five modules. Persists
//     the array as a JSON string through putPluginSetting; the Rhai
//     script reads it via vault_get on every dispatch (so toggle
//     flips take effect on the next tool call without a reload).

import { useCallback, useEffect, useState } from "react";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import type { PluginConfigProps } from "./PluginConfigBase";
import { OauthClientConfig } from "./OauthClientConfig";
import { getPluginSetting, putPluginSetting } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

// Mirror plugins/google-apps/plugin.toml's [[oauth_accounts]] scopes.
// Keep in sync with the manifest — the backend reconciles
// (persisted ∪ manifest) on connect, but the SPA's Save still needs
// the right list to drive the displayed scope set.
const SCOPES = [
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/calendar.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "https://www.googleapis.com/auth/contacts.readonly",
    "https://www.googleapis.com/auth/tasks",
    "https://www.googleapis.com/auth/drive.readonly",
    "https://www.googleapis.com/auth/drive.file",
    "openid",
    "email",
];

type ModuleId = "gmail" | "calendar" | "contacts" | "tasks" | "drive";

const ALL_MODULES: { id: ModuleId; label: string; icon: string }[] = [
    { id: "gmail", label: "Gmail", icon: "bi-envelope" },
    { id: "calendar", label: "Calendar", icon: "bi-calendar3" },
    { id: "contacts", label: "Contacts", icon: "bi-person-vcard" },
    { id: "tasks", label: "Tasks", icon: "bi-check2-square" },
    { id: "drive", label: "Drive", icon: "bi-folder" },
];

const DEFAULT_ENABLED: ModuleId[] = ["gmail", "calendar", "contacts", "tasks", "drive"];

function parseEnabled(raw: string | null): ModuleId[] {
    if (raw === null) return DEFAULT_ENABLED;
    try {
        const parsed = JSON.parse(raw);
        if (!Array.isArray(parsed)) return DEFAULT_ENABLED;
        const out: ModuleId[] = [];
        for (const item of parsed) {
            if (
                typeof item === "string" &&
                (ALL_MODULES.some((m) => m.id === item))
            ) {
                out.push(item as ModuleId);
            }
        }
        return out;
    } catch {
        return DEFAULT_ENABLED;
    }
}

function ModuleToggles({ pluginId }: { pluginId: string }) {
    const { getAccessToken } = useAuth();
    const [enabled, setEnabled] = useState<ModuleId[] | "loading">("loading");
    const [error, setError] = useState<string | null>(null);
    const [saving, setSaving] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const setting = await getPluginSetting(
                pluginId,
                "enabled_modules",
                getAccessToken,
            );
            setEnabled(parseEnabled(setting?.value ?? null));
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setEnabled(DEFAULT_ENABLED);
        }
    }, [getAccessToken, pluginId]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const toggle = useCallback(
        async (id: ModuleId) => {
            if (enabled === "loading") return;
            const next = enabled.includes(id)
                ? enabled.filter((m) => m !== id)
                : [...enabled, id];
            setSaving(true);
            try {
                await putPluginSetting(
                    pluginId,
                    "enabled_modules",
                    JSON.stringify(next),
                    getAccessToken,
                );
                setEnabled(next);
                setError(null);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setSaving(false);
            }
        },
        [enabled, getAccessToken, pluginId],
    );

    return (
        <div
            className="execlaw-card mt-3"
            data-testid="google-apps-module-toggles"
        >
            <div className="execlaw-card__title">
                <i className="bi bi-toggles me-2" aria-hidden />
                Modules
            </div>
            <div className="execlaw-muted small mb-2">
                Toggle individual Google services on or off without
                re-running the OAuth consent flow. Disabled modules
                throw a clear error if the agent tries to invoke their
                tools.
            </div>
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-2"
            />
            {enabled === "loading" ? (
                <Spinner size="sm" animation="border" />
            ) : (
                <div className="d-flex flex-wrap gap-2">
                    {ALL_MODULES.map((m) => {
                        const on = enabled.includes(m.id);
                        return (
                            <Form.Check
                                key={m.id}
                                type="switch"
                                id={`google-apps-toggle-${m.id}`}
                                data-testid={`google-apps-toggle-${m.id}`}
                                label={
                                    <span>
                                        <i
                                            className={`bi ${m.icon} me-1`}
                                            aria-hidden
                                        />
                                        {m.label}
                                    </span>
                                }
                                checked={on}
                                disabled={saving}
                                onChange={() => void toggle(m.id)}
                            />
                        );
                    })}
                </div>
            )}
        </div>
    );
}

export function GoogleAppsPage({
    pluginId,
    onConfigChanged,
}: PluginConfigProps) {
    return (
        <>
            <OauthClientConfig
                pluginId={pluginId}
                provider="google"
                defaultScopes={SCOPES}
                title="Google Apps"
                icon="bi-google"
                description={
                    <>
                        Connects the <code>{pluginId}</code> plugin to your
                        Google account — Gmail, Calendar, Contacts, Tasks,
                        and Drive in one consent screen. Replaces the
                        separate google-calendar + google-contacts plugins.
                        Individual modules can be toggled below without
                        re-running OAuth.
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
                            Enable each API you want to use:{" "}
                            <a
                                href="https://console.cloud.google.com/apis/library/gmail.googleapis.com"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                Gmail
                            </a>
                            ,{" "}
                            <a
                                href="https://console.cloud.google.com/apis/library/calendar-json.googleapis.com"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                Calendar
                            </a>
                            ,{" "}
                            <a
                                href="https://console.cloud.google.com/apis/library/people.googleapis.com"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                People (Contacts)
                            </a>
                            ,{" "}
                            <a
                                href="https://console.cloud.google.com/apis/library/tasks.googleapis.com"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                Tasks
                            </a>
                            , and{" "}
                            <a
                                href="https://console.cloud.google.com/apis/library/drive.googleapis.com"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                Drive
                            </a>
                            .
                        </li>
                        <li>
                            Create an OAuth 2.0 Client ID of type{" "}
                            <strong>Web application</strong>. If you
                            already have one for another Google plugin
                            you can reuse it — Google scopes accumulate
                            per OAuth client.
                        </li>
                        <li>
                            Under Authorized redirect URIs, add this
                            server's callback URL (shown in the form
                            above).
                        </li>
                        <li>
                            Paste the resulting client ID + secret above
                            and click Save, then Connect Account. The
                            consent screen will request the union of all
                            five modules' scopes. To skip a module's
                            scopes, disable it under Modules below and
                            then reconnect.
                        </li>
                    </ol>
                }
                onConfigChanged={onConfigChanged}
            />
            <ModuleToggles pluginId={pluginId} />
        </>
    );
}
