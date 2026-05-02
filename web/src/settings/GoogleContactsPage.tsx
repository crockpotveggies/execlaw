// Settings → Google Contacts plugin config (Phase 9, plugin-google-contacts).
//
// Renders the OAuth client form + Connect/Disconnect dance for the
// `google-contacts` plugin. Mounted by `PluginConfigRouter` inside
// the `PluginConfigShell`, which owns the page header (back button +
// title + version badge) and the danger-zone footer (Uninstall) —
// this component cannot remove either.
//
// Implements the `PluginConfigComponent` contract from
// `PluginConfigBase.tsx` so a future plugin with the same shape
// can be a sibling component without touching the shell.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    connectOauth,
    deleteOauthClient,
    disconnectOauth,
    getOauthClient,
    upsertOauthClient,
    type OauthClientView,
} from "../api/endpoints";
import { ApiError } from "../api/client";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

const ACCOUNT_NAME = "controller";
const PROVIDER = "google";
const DEFAULT_SCOPES: ReadonlyArray<string> = [
    "https://www.googleapis.com/auth/contacts.readonly",
    "openid",
    "email",
];

function defaultRedirectUri(): string {
    if (typeof window === "undefined") return "";
    const origin = window.location.origin;
    return `${origin}/api/oauth/google/callback`;
}

interface FormState {
    client_id: string;
    /// Empty string means "preserve persisted secret" — see
    /// `oauth_admin::upsert_client_handler` for the sentinel.
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
        client_secret: "", // never round-trip; sentinel preserves on save
        redirect_uri: c.redirect_uri,
    };
}

export function GoogleContactsPage(props: PluginConfigProps) {
    const { pluginId, onConfigChanged } = props;
    const { getAccessToken } = useAuth();
    const [client, setClient] = useState<OauthClientView | null | "loading">(
        "loading",
    );
    const [form, setForm] = useState<FormState>(EMPTY_FORM);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState<null | "save" | "connect" | "disconnect" | "delete">(
        null,
    );

    const notifyShell = useCallback(() => {
        if (onConfigChanged) onConfigChanged();
    }, [onConfigChanged]);

    const refresh = useCallback(async () => {
        setError(null);
        // Defensive: a stale shell that mounts us without props
        // would leave pluginId as the JS value `undefined`, which
        // encodeURIComponent stringifies to "undefined" — fires
        // a real /api/admin/oauth/clients/undefined/controller
        // request and gets a 404. Bail out clearly instead.
        if (!pluginId) {
            setError(
                "Plugin id missing from page props — restart the SPA dev server (vite cache may be stale).",
            );
            setClient(null);
            return;
        }
        try {
            const c = await getOauthClient(
                pluginId,
                ACCOUNT_NAME,
                getAccessToken,
            );
            setClient(c);
            setForm(fromRow(c));
        } catch (e) {
            if (e instanceof ApiError && e.status === 404) {
                // Not configured yet — show the form with default
                // redirect URI pre-filled.
                setClient(null);
                setForm({ ...EMPTY_FORM, redirect_uri: defaultRedirectUri() });
            } else {
                setError(
                    e instanceof Error ? e.message : "failed to load client",
                );
                setClient(null);
            }
        }
    }, [getAccessToken, pluginId]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onSave = useCallback(async () => {
        setError(null);
        setBusy("save");
        try {
            const updated = await upsertOauthClient(
                pluginId,
                ACCOUNT_NAME,
                {
                    provider: PROVIDER,
                    client_id: form.client_id.trim(),
                    client_secret: form.client_secret, // empty = preserve
                    redirect_uri: form.redirect_uri.trim(),
                    scopes: [...DEFAULT_SCOPES],
                },
                getAccessToken,
            );
            setClient(updated);
            setForm(fromRow(updated));
            notifyShell();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(null);
        }
    }, [form, getAccessToken, notifyShell, pluginId]);

    const onConnect = useCallback(async () => {
        setError(null);
        setBusy("connect");
        try {
            const r = await connectOauth(pluginId, ACCOUNT_NAME, getAccessToken);
            // Open in a new tab — Google's consent screen lives at
            // accounts.google.com and they reject embedding via X-Frame.
            window.open(r.authorize_url, "_blank", "noopener,noreferrer");
            // Don't poll; let the operator hit Refresh after they
            // close the success tab. A future iteration can listen
            // for postMessage from the callback page.
        } catch (e) {
            setError(e instanceof Error ? e.message : "connect failed");
        } finally {
            setBusy(null);
        }
    }, [getAccessToken, pluginId]);

    const onDisconnect = useCallback(async () => {
        setError(null);
        setBusy("disconnect");
        try {
            await disconnectOauth(pluginId, ACCOUNT_NAME, getAccessToken);
            await refresh();
            notifyShell();
        } catch (e) {
            setError(e instanceof Error ? e.message : "disconnect failed");
        } finally {
            setBusy(null);
        }
    }, [getAccessToken, notifyShell, pluginId, refresh]);

    const onDeleteClient = useCallback(async () => {
        setError(null);
        setBusy("delete");
        try {
            await deleteOauthClient(pluginId, ACCOUNT_NAME, getAccessToken);
            setClient(null);
            setForm({ ...EMPTY_FORM, redirect_uri: defaultRedirectUri() });
            notifyShell();
        } catch (e) {
            setError(e instanceof Error ? e.message : "delete failed");
        } finally {
            setBusy(null);
        }
    }, [getAccessToken, notifyShell, pluginId]);

    if (client === "loading") {
        return (
            <div className="execlaw-muted small">Loading…</div>
        );
    }

    return (
        <div data-testid="google-contacts-page">
            <div className="execlaw-card mb-3">
                <div className="execlaw-card__title">
                    <i className="bi bi-person-vcard me-2" aria-hidden />
                    Google Contacts
                </div>
                <div className="execlaw-muted small">
                    Connects the <code>{pluginId}</code> plugin to your
                    Google account. Saved contacts auto-trust as Contact-class
                    principals; the <code>contacts.list</code> tool becomes
                    available on chat turns.
                </div>
            </div>

            {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

            {client && client.connected && (
                <div
                    className="execlaw-card mb-3"
                    data-testid="google-contacts-connected"
                >
                    <div className="execlaw-card__title">
                        <i className="bi bi-check-circle-fill text-success me-2" aria-hidden />
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
                            {new Date(client.token_expires_at * 1000).toLocaleString()}
                            .
                        </div>
                    )}
                    <div className="d-flex gap-2 mt-2">
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={() => void refresh()}
                            disabled={busy !== null}
                        >
                            Refresh
                        </Button>
                        <Button
                            variant="outline-warning"
                            size="sm"
                            onClick={() => void onDisconnect()}
                            disabled={busy !== null}
                        >
                            {busy === "disconnect" ? "Disconnecting…" : "Disconnect"}
                        </Button>
                    </div>
                </div>
            )}

            {client && !client.connected && (
                <div
                    className="execlaw-card mb-3"
                    data-testid="google-contacts-needs-connect"
                >
                    <div className="execlaw-card__title">
                        <i className="bi bi-info-circle me-2" aria-hidden />
                        Client configured — not connected
                    </div>
                    <div className="execlaw-muted small mb-2">
                        Click <strong>Connect Account</strong> to open Google's
                        consent screen in a new tab. After approving, close the
                        success tab and hit Refresh.
                    </div>
                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onConnect()}
                            disabled={busy !== null}
                            data-testid="connect-button"
                        >
                            {busy === "connect" ? "Opening Google…" : "Connect Account"}
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
                    Google OAuth client credentials
                </summary>
                <div className="execlaw-muted small mt-2 mb-2">
                    Get these from the{" "}
                    <a
                        href="https://console.cloud.google.com/apis/credentials"
                        target="_blank"
                        rel="noopener noreferrer"
                    >
                        Google Cloud Console
                    </a>
                    . Create an OAuth 2.0 Client ID of type "Web application",
                    add this server's callback URL to the authorized redirect
                    URIs, paste the client ID + secret here.
                </div>
                <Form>
                    <Form.Group className="mb-2">
                        <Form.Label className="small">Client ID</Form.Label>
                        <Form.Control
                            size="sm"
                            type="text"
                            value={form.client_id}
                            placeholder="123456789-xxx.apps.googleusercontent.com"
                            onChange={(e) =>
                                setForm({ ...form, client_id: e.target.value })
                            }
                            data-testid="client-id-input"
                        />
                    </Form.Group>
                    <Form.Group className="mb-2">
                        <Form.Label className="small">
                            Client secret{" "}
                            {client && (
                                <span className="execlaw-muted">
                                    (leave blank to keep the saved secret)
                                </span>
                            )}
                        </Form.Label>
                        <Form.Control
                            size="sm"
                            type="password"
                            value={form.client_secret}
                            placeholder={
                                client ? "(unchanged)" : "GOCSPX-…"
                            }
                            onChange={(e) =>
                                setForm({ ...form, client_secret: e.target.value })
                            }
                            data-testid="client-secret-input"
                        />
                    </Form.Group>
                    <Form.Group className="mb-2">
                        <Form.Label className="small">Authorized redirect URI</Form.Label>
                        <Form.Control
                            size="sm"
                            type="url"
                            value={form.redirect_uri}
                            onChange={(e) =>
                                setForm({ ...form, redirect_uri: e.target.value })
                            }
                            data-testid="redirect-uri-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Must match the redirect URI registered in Google
                            Cloud Console exactly.
                        </Form.Text>
                    </Form.Group>
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
                                {busy === "delete" ? "Deleting…" : "Delete client"}
                            </Button>
                        )}
                    </div>
                </Form>
            </details>

            <details className="execlaw-card mt-3">
                <summary>
                    <i className="bi bi-info-square me-2" aria-hidden />
                    Required Google Cloud setup
                </summary>
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
                        Under Authorized redirect URIs, add:
                        <br />
                        <code>{defaultRedirectUri()}</code>
                    </li>
                    <li>
                        Paste the resulting client ID + secret above and click
                        Save, then Connect Account.
                    </li>
                </ol>
            </details>
        </div>
    );
}
