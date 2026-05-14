// Slack plugin self-contained config panel.
//
// Migrated from `web/src/settings/SlackConfigPage.tsx` (2026-05-14).
// Build: node scripts/build-plugin-ui.mjs slack

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface SlackWorkspaceView {
    team_id: string;
    team_name: string;
    bot_user_id: string;
    controller_user_id: string;
    bot_token_masked: string;
    app_token_masked: string;
}

interface SlackWorkspacesResponse {
    workspaces: SlackWorkspaceView[];
}

interface SlackStatusResponse {
    sidecar_status: string;
    sidecar_rpc_url: string | null;
    registered_accounts: string[];
    accounts_on_disk: string[];
    fetch_error: string | null;
    workspaces_configured: number;
}

interface SlackAddWorkspaceResponse {
    team_id: string;
    team_name: string;
    bot_user_id: string;
    controller_user_id: string;
}

interface SlackTestResponse {
    ok?: boolean;
    ts?: string;
    error?: string;
}

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge } = props;
    const { ErrorBanner, Button } = bridge.components;

    const [, setStatus] = useState<SlackStatusResponse | null>(null);
    const [workspaces, setWorkspaces] = useState<SlackWorkspaceView[] | null>(
        null,
    );
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [showAddForm, setShowAddForm] = useState(false);
    const [botToken, setBotToken] = useState("");
    const [appToken, setAppToken] = useState("");
    const [controllerUserId, setControllerUserId] = useState("");
    const [savedNotice, setSavedNotice] = useState<string | null>(null);
    const [testTeamId, setTestTeamId] = useState<string | null>(null);
    const [testChannel, setTestChannel] = useState("");
    const [testStatus, setTestStatus] = useState<
        | { kind: "idle" }
        | { kind: "ok"; message: string }
        | { kind: "err"; message: string }
    >({ kind: "idle" });

    const reload = useCallback(async () => {
        try {
            const [s, w] = await Promise.all([
                bridge.fetchJson<SlackStatusResponse>(
                    "GET",
                    "/api/admin/plugins/slack/status",
                ),
                bridge.fetchJson<SlackWorkspacesResponse>(
                    "GET",
                    "/api/admin/plugins/slack/workspaces",
                ),
            ]);
            setStatus(s);
            setWorkspaces(w.workspaces);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [bridge]);

    useEffect(() => {
        void reload();
    }, [reload]);

    const onAdd = useCallback(async () => {
        if (!botToken.trim().startsWith("xoxb-")) {
            setError("Bot token must start with xoxb-");
            return;
        }
        if (!appToken.trim().startsWith("xapp-")) {
            setError("App token must start with xapp-");
            return;
        }
        const trimmedController = controllerUserId.trim();
        setBusy(true);
        setError(null);
        setSavedNotice(null);
        try {
            const added = await bridge.fetchJson<SlackAddWorkspaceResponse>(
                "POST",
                "/api/admin/plugins/slack/workspaces",
                {
                    bot_token: botToken.trim(),
                    app_token: appToken.trim(),
                    controller_user_id: trimmedController,
                },
            );
            if (trimmedController !== "") {
                try {
                    await bridge.fetchJson<unknown>(
                        "POST",
                        "/api/admin/me/identifiers",
                        { transport: "slack", handle: trimmedController },
                    );
                } catch (idErr) {
                    setError(
                        `Workspace saved, but couldn't register the controller identity: ${
                            idErr instanceof Error
                                ? idErr.message
                                : String(idErr)
                        }. Add it manually via Settings → My Identities.`,
                    );
                }
            }
            setSavedNotice(
                `Workspace ${added.team_name} (${added.team_id}) added.` +
                    (trimmedController === ""
                        ? " You can add the controller's Slack user id later — paste another workspace or edit this one."
                        : ` Bot user: ${added.bot_user_id}. Controller identity slack:${trimmedController} registered.`),
            );
            setBotToken("");
            setAppToken("");
            setControllerUserId("");
            setShowAddForm(false);
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [appToken, botToken, controllerUserId, bridge, reload]);

    const onRemove = useCallback(
        async (team_id: string) => {
            if (
                !window.confirm(
                    `Remove workspace ${team_id}? The bot+app tokens will be deleted from the vault and the Socket Mode connection drops on next plugin reload.`,
                )
            ) {
                return;
            }
            setBusy(true);
            setError(null);
            try {
                const qs = new URLSearchParams({ team_id }).toString();
                await bridge.fetchJson<unknown>(
                    "DELETE",
                    `/api/admin/plugins/slack/workspaces?${qs}`,
                );
                await reload();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [bridge, reload],
    );

    const onTest = useCallback(async () => {
        if (!testTeamId) return;
        if (!testChannel.trim()) {
            setTestStatus({ kind: "err", message: "channel id required" });
            return;
        }
        setBusy(true);
        setTestStatus({ kind: "idle" });
        try {
            const r = await bridge.fetchJson<SlackTestResponse>(
                "POST",
                "/api/admin/plugins/slack/test",
                { team_id: testTeamId, channel: testChannel.trim() },
            );
            if (r.ok) {
                setTestStatus({
                    kind: "ok",
                    message: `Sent. Slack ts: ${r.ts ?? "(none)"}`,
                });
            } else {
                setTestStatus({
                    kind: "err",
                    message: r.error ?? "Slack rejected the test message.",
                });
            }
        } catch (e) {
            setTestStatus({
                kind: "err",
                message: e instanceof Error ? e.message : String(e),
            });
        } finally {
            setBusy(false);
        }
    }, [bridge, testChannel, testTeamId]);

    if (workspaces === null) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <span
                    className="spinner-border spinner-border-sm me-2"
                    role="status"
                    aria-hidden
                />
                Loading…
            </div>
        );
    }

    const wsCount = workspaces.length;
    const statusBadgeClass =
        wsCount > 0 ? "bg-success" : "bg-warning text-dark";
    const statusLabel = wsCount > 0 ? "configured" : "unconfigured";

    return (
        <div data-testid="slack-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <div className="card mb-3">
                <div className="card-body">
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Slack workspaces</h5>
                        <span
                            className={`badge ${statusBadgeClass}`}
                            data-testid="slack-status-badge"
                        >
                            {statusLabel}
                        </span>
                        <span className="execlaw-muted small ms-2">
                            {wsCount} workspace{wsCount === 1 ? "" : "s"}
                        </span>
                    </div>
                    <p className="execlaw-muted small mb-3">
                        Each workspace connects via Socket Mode (no public URL
                        required) using a bot token (<code>xoxb-</code>) plus
                        an app-level token (<code>xapp-</code>). Add a new app
                        at{" "}
                        <a
                            href="https://api.slack.com/apps"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            api.slack.com/apps
                        </a>
                        : enable Socket Mode → generate an App-Level Token
                        (events scope) → install to workspace → copy the Bot
                        User OAuth Token from <em>OAuth &amp; Permissions</em>.
                        Required scopes:{" "}
                        <code>
                            chat:write, channels:history, groups:history,
                            im:history, channels:read, groups:read, users:read,
                            files:write
                        </code>
                        .
                    </p>

                    {savedNotice && (
                        <div className="alert alert-success" data-testid="slack-saved">
                            {savedNotice}
                        </div>
                    )}

                    {wsCount === 0 ? (
                        <div className="execlaw-muted small mb-3">
                            No workspaces yet. Add one to start receiving
                            Slack messages.
                        </div>
                    ) : (
                        <table
                            className="table table-sm mb-3"
                            data-testid="slack-workspace-table"
                        >
                            <thead>
                                <tr>
                                    <th>Team</th>
                                    <th>Bot user</th>
                                    <th>Controller user</th>
                                    <th>Bot token</th>
                                    <th>App token</th>
                                    <th />
                                </tr>
                            </thead>
                            <tbody>
                                {workspaces.map((ws: SlackWorkspaceView) => (
                                    <tr
                                        key={ws.team_id}
                                        data-testid={`slack-workspace-row-${ws.team_id}`}
                                    >
                                        <td>
                                            <strong>{ws.team_name}</strong>
                                            <br />
                                            <code className="execlaw-muted small">
                                                {ws.team_id}
                                            </code>
                                        </td>
                                        <td>
                                            <code className="execlaw-muted small">
                                                {ws.bot_user_id}
                                            </code>
                                        </td>
                                        <td>
                                            {ws.controller_user_id ? (
                                                <code className="small">
                                                    {ws.controller_user_id}
                                                </code>
                                            ) : (
                                                <span className="execlaw-muted small">
                                                    (not set)
                                                </span>
                                            )}
                                        </td>
                                        <td>
                                            <code className="execlaw-muted small">
                                                {ws.bot_token_masked}
                                            </code>
                                        </td>
                                        <td>
                                            <code className="execlaw-muted small">
                                                {ws.app_token_masked}
                                            </code>
                                        </td>
                                        <td>
                                            <div className="d-flex gap-2">
                                                <Button
                                                    size="sm"
                                                    variant="outline-secondary"
                                                    onClick={() => {
                                                        setTestTeamId(ws.team_id);
                                                        setTestChannel("");
                                                        setTestStatus({ kind: "idle" });
                                                    }}
                                                    data-testid={`slack-test-${ws.team_id}`}
                                                >
                                                    Test
                                                </Button>
                                                <Button
                                                    size="sm"
                                                    variant="outline-danger"
                                                    onClick={() =>
                                                        void onRemove(ws.team_id)
                                                    }
                                                    disabled={busy}
                                                    data-testid={`slack-remove-${ws.team_id}`}
                                                >
                                                    Remove
                                                </Button>
                                            </div>
                                        </td>
                                    </tr>
                                ))}
                            </tbody>
                        </table>
                    )}

                    {!showAddForm ? (
                        <Button
                            size="sm"
                            variant="primary"
                            onClick={() => setShowAddForm(true)}
                            data-testid="slack-add-workspace"
                        >
                            <i className="bi bi-plus-lg me-1" aria-hidden />
                            Add workspace
                        </Button>
                    ) : (
                        <div
                            className="border-top pt-3 mt-2"
                            data-testid="slack-add-form"
                        >
                            <div className="mb-2">
                                <label className="form-label execlaw-muted small mb-1">
                                    Bot token
                                </label>
                                <input
                                    type="password"
                                    className="form-control"
                                    placeholder="xoxb-..."
                                    value={botToken}
                                    onChange={(e: { target: { value: string } }) =>
                                        setBotToken(e.target.value)
                                    }
                                    data-testid="slack-bot-token-input"
                                />
                            </div>
                            <div className="mb-2">
                                <label className="form-label execlaw-muted small mb-1">
                                    App-level token
                                </label>
                                <input
                                    type="password"
                                    className="form-control"
                                    placeholder="xapp-..."
                                    value={appToken}
                                    onChange={(e: { target: { value: string } }) =>
                                        setAppToken(e.target.value)
                                    }
                                    data-testid="slack-app-token-input"
                                />
                            </div>
                            <div className="mb-3">
                                <label className="form-label execlaw-muted small mb-1">
                                    Controller Slack user id
                                    <span className="execlaw-muted ms-1">
                                        (optional)
                                    </span>
                                </label>
                                <input
                                    type="text"
                                    className="form-control"
                                    placeholder="U0A5GB3BJFL"
                                    value={controllerUserId}
                                    onChange={(e: { target: { value: string } }) =>
                                        setControllerUserId(e.target.value)
                                    }
                                    data-testid="slack-controller-user-input"
                                />
                                <div className="form-text execlaw-muted">
                                    Your own Slack user id in this workspace
                                    (different from the bot&apos;s id). Inbound
                                    DMs from this user resolve to the
                                    controller via My Identities, skipping
                                    the cold-contact ladder. Find it: click
                                    your avatar in Slack → View profile → ⋮ →
                                    Copy member ID.
                                </div>
                            </div>
                            <div className="d-flex gap-2">
                                <Button
                                    size="sm"
                                    variant="primary"
                                    onClick={() => void onAdd()}
                                    disabled={
                                        busy ||
                                        botToken.trim() === "" ||
                                        appToken.trim() === ""
                                    }
                                    data-testid="slack-add-save"
                                >
                                    Save
                                </Button>
                                <Button
                                    size="sm"
                                    variant="outline-secondary"
                                    onClick={() => {
                                        setShowAddForm(false);
                                        setBotToken("");
                                        setAppToken("");
                                        setControllerUserId("");
                                    }}
                                    disabled={busy}
                                >
                                    Cancel
                                </Button>
                            </div>
                        </div>
                    )}
                </div>
            </div>

            {testTeamId && (
                <div className="card mb-3" data-testid="slack-test-card">
                    <div className="card-body">
                        <h5 className="h6 mb-2">Test message</h5>
                        <p className="execlaw-muted small mb-2">
                            Send a one-shot message to confirm the bot is
                            wired and has permission to post in the target
                            channel. The bot must be a member of the channel
                            (right-click channel → View channel details →
                            Integrations → Add apps).
                        </p>
                        <div className="mb-2">
                            <label className="form-label execlaw-muted small mb-1">
                                Channel id (workspace: <code>{testTeamId}</code>)
                            </label>
                            <input
                                type="text"
                                className="form-control"
                                placeholder="C0123456789"
                                value={testChannel}
                                onChange={(e: { target: { value: string } }) =>
                                    setTestChannel(e.target.value)
                                }
                                onKeyDown={(e: {
                                    key: string;
                                    preventDefault: () => void;
                                }) => {
                                    if (e.key === "Enter") {
                                        e.preventDefault();
                                        void onTest();
                                    }
                                }}
                                autoFocus
                                data-testid="slack-test-channel-input"
                            />
                        </div>
                        <div className="d-flex gap-2">
                            <Button
                                size="sm"
                                variant="primary"
                                onClick={() => void onTest()}
                                disabled={busy}
                                data-testid="slack-test-send"
                            >
                                Send test message
                            </Button>
                            <Button
                                size="sm"
                                variant="outline-secondary"
                                onClick={() => {
                                    setTestTeamId(null);
                                    setTestChannel("");
                                    setTestStatus({ kind: "idle" });
                                }}
                                disabled={busy}
                            >
                                Close
                            </Button>
                        </div>
                        {testStatus.kind === "ok" && (
                            <div
                                className="alert alert-success mt-2"
                                data-testid="slack-test-ok"
                            >
                                {testStatus.message}
                            </div>
                        )}
                        {testStatus.kind === "err" && (
                            <div
                                className="alert alert-danger mt-2"
                                data-testid="slack-test-err"
                            >
                                {testStatus.message}
                            </div>
                        )}
                    </div>
                </div>
            )}
        </div>
    );
};

export default Panel;
