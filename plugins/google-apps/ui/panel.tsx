// Google Apps plugin self-contained config panel.
//
// Migrated from `web/src/settings/GoogleAppsPage.tsx` (2026-05-14):
// consolidated Google plugin — Gmail, Calendar, Contacts, Tasks, and
// Drive in one OAuth grant. The shared OauthClientConfig component
// is inlined below (the host SPA used to ship a reusable copy; for
// self-containment we keep one in each Google plugin's bundle).
//
// Build: node scripts/build-plugin-ui.mjs google-apps

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

type ReactNode = ReturnType<PluginPanelComponent>;

// --- OAuth API types & helpers (inlined from web/src/api/endpoints.ts) ---

interface OauthClientView {
    plugin_id: string;
    account_name: string;
    provider: string;
    client_id: string;
    redirect_uri: string;
    scopes: string[];
    created_at: number;
    updated_at: number;
    connected: boolean;
    account_email: string | null;
    token_expires_at: number | null;
}

interface ConnectOauthResponse {
    authorize_url: string;
}

interface PluginSettingView {
    plugin_id: string;
    key: string;
    value: string;
}

// Helper: detect HTTP 404 surface from bridge.fetchJson (which throws
// an Error whose message embeds `→ 404:`). Used by getOauthClient to
// distinguish "no client configured yet" from genuine failure.
function isNotFoundError(e: unknown): boolean {
    if (!(e instanceof Error)) return false;
    return /→\s*404\b/.test(e.message);
}

// --- Plugin-specific constants ---

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

const DEFAULT_ENABLED: ModuleId[] = [
    "gmail",
    "calendar",
    "contacts",
    "tasks",
    "drive",
];

function parseEnabled(raw: string | null): ModuleId[] {
    if (raw === null) return DEFAULT_ENABLED;
    try {
        const parsed = JSON.parse(raw);
        if (!Array.isArray(parsed)) return DEFAULT_ENABLED;
        const out: ModuleId[] = [];
        for (const item of parsed) {
            if (
                typeof item === "string" &&
                ALL_MODULES.some((m: { id: ModuleId }) => m.id === item)
            ) {
                out.push(item as ModuleId);
            }
        }
        return out;
    } catch {
        return DEFAULT_ENABLED;
    }
}

// --- Panel ----------------------------------------------------------

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { identity } = props;
    return (
        <>
            <OauthClientConfig
                pluginId={identity.id}
                bridge={props.bridge}
                provider="google"
                defaultScopes={SCOPES}
                title="Google Apps"
                icon="bi-google"
                description={
                    <>
                        Connects the <code>{identity.id}</code> plugin to your
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
                            server&apos;s callback URL (shown in the form
                            above).
                        </li>
                        <li>
                            Paste the resulting client ID + secret above
                            and click Save, then Connect Account. The
                            consent screen will request the union of all
                            five modules&apos; scopes. To skip a module&apos;s
                            scopes, disable it under Modules below and
                            then reconnect.
                        </li>
                    </ol>
                }
            />
            <ModuleToggles pluginId={identity.id} bridge={props.bridge} />
        </>
    );
};

export default Panel;

// --- ModuleToggles --------------------------------------------------

function ModuleToggles({
    pluginId,
    bridge,
}: {
    pluginId: string;
    bridge: PluginPanelProps["bridge"];
}) {
    const { ErrorBanner } = bridge.components;
    const [enabled, setEnabled] = useState<ModuleId[] | "loading">("loading");
    const [error, setError] = useState<string | null>(null);
    const [saving, setSaving] = useState(false);

    const refresh = useCallback(async () => {
        try {
            let value: string | null = null;
            try {
                const setting = await bridge.fetchJson<PluginSettingView>(
                    "GET",
                    `/api/admin/plugins/${encodeURIComponent(pluginId)}/settings/enabled_modules`,
                );
                value = setting?.value ?? null;
            } catch (e) {
                if (!isNotFoundError(e)) throw e;
            }
            setEnabled(parseEnabled(value));
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setEnabled(DEFAULT_ENABLED);
        }
    }, [bridge, pluginId]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const toggle = useCallback(
        async (id: ModuleId) => {
            if (enabled === "loading") return;
            const next = enabled.includes(id)
                ? enabled.filter((m: ModuleId) => m !== id)
                : [...enabled, id];
            setSaving(true);
            try {
                await bridge.fetchJson<PluginSettingView>(
                    "PUT",
                    `/api/admin/plugins/${encodeURIComponent(pluginId)}/settings/enabled_modules`,
                    { value: JSON.stringify(next) },
                );
                setEnabled(next);
                setError(null);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setSaving(false);
            }
        },
        [enabled, bridge, pluginId],
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
                <span
                    className="spinner-border spinner-border-sm"
                    role="status"
                    aria-hidden
                />
            ) : (
                <div className="d-flex flex-wrap gap-3">
                    {ALL_MODULES.map(
                        (m: { id: ModuleId; label: string; icon: string }) => {
                            const on = enabled.includes(m.id);
                            return (
                                <div
                                    key={m.id}
                                    className="form-check form-switch"
                                >
                                    <input
                                        type="checkbox"
                                        role="switch"
                                        className="form-check-input"
                                        id={`google-apps-toggle-${m.id}`}
                                        data-testid={`google-apps-toggle-${m.id}`}
                                        checked={on}
                                        disabled={saving}
                                        onChange={() => void toggle(m.id)}
                                    />
                                    <label
                                        htmlFor={`google-apps-toggle-${m.id}`}
                                        className="form-check-label"
                                    >
                                        <i
                                            className={`bi ${m.icon} me-1`}
                                            aria-hidden
                                        />
                                        {m.label}
                                    </label>
                                </div>
                            );
                        },
                    )}
                </div>
            )}
        </div>
    );
}

// --- OauthClientConfig (inlined from web/src/settings/OauthClientConfig.tsx) ---

interface FormState {
    client_id: string;
    /** Empty string = "preserve persisted secret" sentinel. */
    client_secret: string;
    redirect_uri: string;
}

const EMPTY_FORM: FormState = {
    client_id: "",
    client_secret: "",
    redirect_uri: "",
};

function fromRow(c: OauthClientView): FormState {
    return {
        client_id: c.client_id,
        client_secret: "",
        redirect_uri: c.redirect_uri,
    };
}

function defaultRedirectUri(provider: string): string {
    if (typeof window === "undefined") return "";
    return `${window.location.origin}/api/oauth/${provider}/callback`;
}

interface OauthClientConfigProps {
    pluginId: string;
    bridge: PluginPanelProps["bridge"];
    accountName?: string;
    provider: "google";
    defaultScopes: string[];
    title: string;
    icon: string;
    description: ReactNode;
    setupSteps: ReactNode;
}

function OauthClientConfig(props: OauthClientConfigProps) {
    const {
        pluginId,
        bridge,
        accountName = "controller",
        provider,
        defaultScopes,
        title,
        icon,
        description,
        setupSteps,
    } = props;
    const { ErrorBanner, Button } = bridge.components;

    const [client, setClient] = useState<OauthClientView | null | "loading">(
        "loading",
    );
    const [form, setForm] = useState<FormState>(EMPTY_FORM);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState<
        null | "save" | "connect" | "disconnect" | "delete"
    >(null);

    const refresh = useCallback(async () => {
        setError(null);
        if (!pluginId) {
            setError(
                "Plugin id missing from page props — restart the SPA dev server (vite cache may be stale).",
            );
            setClient(null);
            return;
        }
        try {
            const c = await bridge.fetchJson<OauthClientView>(
                "GET",
                `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}`,
            );
            setClient(c);
            setForm(fromRow(c));
        } catch (e) {
            if (isNotFoundError(e)) {
                setClient(null);
                setForm({
                    ...EMPTY_FORM,
                    redirect_uri: defaultRedirectUri(provider),
                });
            } else {
                setError(
                    e instanceof Error ? e.message : "failed to load client",
                );
                setClient(null);
            }
        }
    }, [accountName, bridge, pluginId, provider]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onSave = useCallback(async () => {
        setError(null);
        setBusy("save");
        try {
            const updated = await bridge.fetchJson<OauthClientView>(
                "PUT",
                `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}`,
                {
                    provider,
                    client_id: form.client_id.trim(),
                    client_secret: form.client_secret, // empty = preserve
                    redirect_uri: form.redirect_uri.trim(),
                    scopes: [...defaultScopes],
                },
            );
            setClient(updated);
            setForm(fromRow(updated));
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, defaultScopes, form, bridge, pluginId, provider]);

    const onConnect = useCallback(async () => {
        setError(null);
        setBusy("connect");
        try {
            const r = await bridge.fetchJson<ConnectOauthResponse>(
                "POST",
                `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}/connect`,
            );
            window.open(r.authorize_url, "_blank", "noopener,noreferrer");
        } catch (e) {
            setError(e instanceof Error ? e.message : "connect failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, bridge, pluginId]);

    const onDisconnect = useCallback(async () => {
        setError(null);
        setBusy("disconnect");
        try {
            await bridge.fetchJson<unknown>(
                "POST",
                `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}/disconnect`,
            );
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : "disconnect failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, bridge, pluginId, refresh]);

    const onDeleteClient = useCallback(async () => {
        setError(null);
        setBusy("delete");
        try {
            await bridge.fetchJson<unknown>(
                "DELETE",
                `/api/admin/oauth/clients/${encodeURIComponent(pluginId)}/${encodeURIComponent(accountName)}`,
            );
            setClient(null);
            setForm({
                ...EMPTY_FORM,
                redirect_uri: defaultRedirectUri(provider),
            });
        } catch (e) {
            setError(e instanceof Error ? e.message : "delete failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, bridge, pluginId, provider]);

    if (client === "loading") {
        return <div className="execlaw-muted small">Loading…</div>;
    }

    return (
        <div data-testid="oauth-client-config" data-plugin-id={pluginId}>
            <div className="execlaw-card mb-3">
                <div className="execlaw-card__title">
                    <i className={`bi ${icon} me-2`} aria-hidden />
                    {title}
                </div>
                <div className="execlaw-muted small">{description}</div>
            </div>

            {error && (
                <ErrorBanner
                    message={error}
                    onDismiss={() => setError(null)}
                />
            )}

            {client && client.connected && (
                <div
                    className="execlaw-card mb-3"
                    data-testid="oauth-connected"
                >
                    <div className="execlaw-card__title">
                        <i
                            className="bi bi-check-circle-fill text-success me-2"
                            aria-hidden
                        />
                        Connected
                        {client.account_email && (
                            <span className="ms-1">
                                as <strong>{client.account_email}</strong>
                            </span>
                        )}
                    </div>
                    {client.token_expires_at && (
                        <div className="execlaw-muted small">
                            Access token refreshes automatically — currently
                            valid until{" "}
                            {new Date(
                                client.token_expires_at * 1000,
                            ).toLocaleString()}
                            .
                        </div>
                    )}
                    <div className="d-flex gap-2 mt-2 flex-wrap">
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={() => void refresh()}
                            disabled={busy !== null}
                        >
                            Refresh
                        </Button>
                        <Button
                            variant="outline-primary"
                            size="sm"
                            onClick={() => void onConnect()}
                            disabled={busy !== null}
                            data-testid="reauthorize-button"
                        >
                            {busy === "connect"
                                ? "Opening…"
                                : "Re-authorize"}
                        </Button>
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={() => void onDisconnect()}
                            disabled={busy !== null}
                        >
                            {busy === "disconnect"
                                ? "Disconnecting…"
                                : "Disconnect"}
                        </Button>
                    </div>
                </div>
            )}

            {client && !client.connected && (
                <div
                    className="execlaw-card mb-3"
                    data-testid="oauth-needs-connect"
                >
                    <div className="execlaw-card__title">
                        <i className="bi bi-info-circle me-2" aria-hidden />
                        Client configured — not connected
                    </div>
                    <div className="execlaw-muted small mb-2">
                        Click <strong>Connect Account</strong> to open the
                        provider&apos;s consent screen in a new tab. After
                        approving, close the success tab and hit Refresh.
                    </div>
                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onConnect()}
                            disabled={busy !== null}
                            data-testid="connect-button"
                        >
                            {busy === "connect"
                                ? "Opening…"
                                : "Connect Account"}
                        </Button>
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={() => void refresh()}
                            disabled={busy !== null}
                        >
                            Refresh
                        </Button>
                    </div>
                </div>
            )}

            <details className="execlaw-card" open={!client}>
                <summary>
                    <i className="bi bi-key me-2" aria-hidden />
                    OAuth client credentials
                </summary>
                <div className="execlaw-muted small mt-2 mb-2">
                    Get these from your provider&apos;s developer console. Create
                    an OAuth 2.0 client of type &quot;Web application&quot;, add this
                    server&apos;s callback URL to the authorized redirect URIs,
                    paste the client ID + secret here.
                </div>
                <form onSubmit={(e: { preventDefault: () => void }) => e.preventDefault()}>
                    <div className="mb-2">
                        <label className="form-label small">Client ID</label>
                        <input
                            type="text"
                            className="form-control form-control-sm"
                            value={form.client_id}
                            placeholder="123456789-xxx.apps.googleusercontent.com"
                            onChange={(e: { target: { value: string } }) =>
                                setForm({
                                    ...form,
                                    client_id: e.target.value,
                                })
                            }
                            data-testid="client-id-input"
                        />
                    </div>
                    <div className="mb-2">
                        <label className="form-label small">
                            Client secret{" "}
                            {client && (
                                <span className="execlaw-muted">
                                    (leave blank to keep the saved secret)
                                </span>
                            )}
                        </label>
                        <input
                            type="password"
                            className="form-control form-control-sm"
                            value={form.client_secret}
                            placeholder={client ? "(unchanged)" : "GOCSPX-…"}
                            onChange={(e: { target: { value: string } }) =>
                                setForm({
                                    ...form,
                                    client_secret: e.target.value,
                                })
                            }
                            data-testid="client-secret-input"
                        />
                    </div>
                    <div className="mb-2">
                        <label className="form-label small">
                            Authorized redirect URI
                        </label>
                        <input
                            type="url"
                            className="form-control form-control-sm"
                            value={form.redirect_uri}
                            onChange={(e: { target: { value: string } }) =>
                                setForm({
                                    ...form,
                                    redirect_uri: e.target.value,
                                })
                            }
                            data-testid="redirect-uri-input"
                        />
                        <div className="form-text execlaw-muted">
                            Must match the redirect URI registered in your
                            provider console exactly.
                        </div>
                    </div>
                    <div className="d-flex gap-2 mt-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={
                                busy !== null ||
                                form.client_id.trim() === "" ||
                                form.redirect_uri.trim() === ""
                            }
                            data-testid="save-button"
                        >
                            {busy === "save" ? "Saving…" : "Save"}
                        </Button>
                        {client && (
                            <Button
                                variant="outline-danger"
                                size="sm"
                                onClick={() => {
                                    if (
                                        window.confirm(
                                            "Delete client config + tokens? You'll need to re-paste the secret to reconnect.",
                                        )
                                    ) {
                                        void onDeleteClient();
                                    }
                                }}
                                disabled={busy !== null}
                            >
                                {busy === "delete"
                                    ? "Deleting…"
                                    : "Delete client"}
                            </Button>
                        )}
                    </div>
                </form>
            </details>

            <details className="execlaw-card mt-3">
                <summary>
                    <i className="bi bi-info-square me-2" aria-hidden />
                    Provider setup steps
                </summary>
                {setupSteps}
            </details>
        </div>
    );
}
