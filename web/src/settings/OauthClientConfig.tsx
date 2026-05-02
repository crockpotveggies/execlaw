// Reusable OAuth client + token management UI for plugin config
// pages. Every Rhai plugin that declares `[[oauth_accounts]]`
// needs the same lifecycle:
//
//   1. paste OAuth client_id / client_secret / redirect URI
//   2. Save
//   3. Connect Account → opens provider consent in a new tab
//   4. Display "Connected as <email>" + Disconnect / Refresh
//
// This component owns all of that. Plugin-specific pages
// (GoogleContactsPage / GoogleCalendarPage / future plugins) just
// pass in the plugin-specific copy (title, icon, description,
// scopes, setup-steps slot) and the generic component does the
// rest. No more 400-line near-duplicate config pages.
//
// Sentinels worth knowing:
//   * `client_secret = ""` on save → backend preserves the
//     persisted secret (see oauth_admin::upsert_client_handler).
//     Lets the SPA pre-populate the form without making the
//     operator re-paste a 40-char secret on every Save.

import { useCallback, useEffect, useState, type ReactNode } from "react";
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

/// Provider id used both in the OAuth admin endpoint paths and in
/// the default redirect URI. Today execlaw only ships a Google
/// provider; when Microsoft / GitHub etc. land they become
/// additional values here.
type ProviderId = "google";

export interface OauthClientConfigProps {
    pluginId: string;
    /// Account name within the plugin's `[[oauth_accounts]]`
    /// declaration. Default "controller" (only one account per
    /// plugin in practice today).
    accountName?: string;
    provider: ProviderId;
    /// Scopes the plugin manifest declared. Saved as-is on every
    /// upsert; the operator can't override them via the SPA
    /// (the manifest is the source of truth).
    defaultScopes: string[];
    /// Plugin-facing title — "Google Contacts" / "Google Calendar" / etc.
    title: string;
    /// Bootstrap-icons class for the title's leading icon.
    icon: string;
    /// One-line "what this plugin does" copy under the title.
    description: ReactNode;
    /// Plugin-specific Cloud Console setup checklist (e.g. which
    /// API to enable). Rendered inside an <ol> beneath the form.
    setupSteps: ReactNode;
    /// Notify the parent shell after a persisted change so the
    /// row badge / version display can re-fetch.
    onConfigChanged?: () => void;
}

interface FormState {
    client_id: string;
    /// Empty string = "preserve persisted secret" sentinel.
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

function defaultRedirectUri(provider: ProviderId): string {
    if (typeof window === "undefined") return "";
    return `${window.location.origin}/api/oauth/${provider}/callback`;
}

export function OauthClientConfig(props: OauthClientConfigProps) {
    const {
        pluginId,
        accountName = "controller",
        provider,
        defaultScopes,
        title,
        icon,
        description,
        setupSteps,
        onConfigChanged,
    } = props;
    const { getAccessToken } = useAuth();

    const [client, setClient] = useState<OauthClientView | null | "loading">(
        "loading",
    );
    const [form, setForm] = useState<FormState>(EMPTY_FORM);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState<
        null | "save" | "connect" | "disconnect" | "delete"
    >(null);

    const notifyShell = useCallback(() => {
        if (onConfigChanged) onConfigChanged();
    }, [onConfigChanged]);

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
            const c = await getOauthClient(pluginId, accountName, getAccessToken);
            setClient(c);
            setForm(fromRow(c));
        } catch (e) {
            if (e instanceof ApiError && e.status === 404) {
                setClient(null);
                setForm({
                    ...EMPTY_FORM,
                    redirect_uri: defaultRedirectUri(provider),
                });
            } else {
                setError(e instanceof Error ? e.message : "failed to load client");
                setClient(null);
            }
        }
    }, [accountName, getAccessToken, pluginId, provider]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onSave = useCallback(async () => {
        setError(null);
        setBusy("save");
        try {
            const updated = await upsertOauthClient(
                pluginId,
                accountName,
                {
                    provider,
                    client_id: form.client_id.trim(),
                    client_secret: form.client_secret, // empty = preserve
                    redirect_uri: form.redirect_uri.trim(),
                    scopes: [...defaultScopes],
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
    }, [
        accountName,
        defaultScopes,
        form,
        getAccessToken,
        notifyShell,
        pluginId,
        provider,
    ]);

    const onConnect = useCallback(async () => {
        setError(null);
        setBusy("connect");
        try {
            const r = await connectOauth(pluginId, accountName, getAccessToken);
            window.open(r.authorize_url, "_blank", "noopener,noreferrer");
        } catch (e) {
            setError(e instanceof Error ? e.message : "connect failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, getAccessToken, pluginId]);

    const onDisconnect = useCallback(async () => {
        setError(null);
        setBusy("disconnect");
        try {
            await disconnectOauth(pluginId, accountName, getAccessToken);
            await refresh();
            notifyShell();
        } catch (e) {
            setError(e instanceof Error ? e.message : "disconnect failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, getAccessToken, notifyShell, pluginId, refresh]);

    const onDeleteClient = useCallback(async () => {
        setError(null);
        setBusy("delete");
        try {
            await deleteOauthClient(pluginId, accountName, getAccessToken);
            setClient(null);
            setForm({
                ...EMPTY_FORM,
                redirect_uri: defaultRedirectUri(provider),
            });
            notifyShell();
        } catch (e) {
            setError(e instanceof Error ? e.message : "delete failed");
        } finally {
            setBusy(null);
        }
    }, [accountName, getAccessToken, notifyShell, pluginId, provider]);

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
                <ErrorBanner message={error} onDismiss={() => setError(null)} />
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
                        provider's consent screen in a new tab. After
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
                    Get these from your provider's developer console. Create
                    an OAuth 2.0 client of type "Web application", add this
                    server's callback URL to the authorized redirect URIs,
                    paste the client ID + secret here.
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
                                setForm({
                                    ...form,
                                    client_id: e.target.value,
                                })
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
                            placeholder={client ? "(unchanged)" : "GOCSPX-…"}
                            onChange={(e) =>
                                setForm({
                                    ...form,
                                    client_secret: e.target.value,
                                })
                            }
                            data-testid="client-secret-input"
                        />
                    </Form.Group>
                    <Form.Group className="mb-2">
                        <Form.Label className="small">
                            Authorized redirect URI
                        </Form.Label>
                        <Form.Control
                            size="sm"
                            type="url"
                            value={form.redirect_uri}
                            onChange={(e) =>
                                setForm({
                                    ...form,
                                    redirect_uri: e.target.value,
                                })
                            }
                            data-testid="redirect-uri-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Must match the redirect URI registered in your
                            provider console exactly.
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
                                {busy === "delete"
                                    ? "Deleting…"
                                    : "Delete client"}
                            </Button>
                        )}
                    </div>
                </Form>
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
