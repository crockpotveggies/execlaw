// Settings → Plugins → Slack.
//
// Multi-workspace from day 1. Form-style (no QR pairing) — operator
// pastes a bot token (xoxb-) + app-level token (xapp-) per
// workspace. The plugin's POST /workspaces handler probes
// auth.test + apps.connections.open before saving so a bad token
// surfaces immediately.
//
// After saving a workspace, the SPA also POSTs the controller
// user id to /api/admin/me/identifiers (transport=slack, handle=U…)
// so the host's principal-admit pipeline treats inbound messages
// from that Slack user as Controller-class.

import { useCallback, useEffect, useState } from "react";
import { Alert, Badge, Button, Card, Form, Spinner, Table } from "react-bootstrap";
import {
    addMyIdentifier,
    addSlackWorkspace,
    getSlackStatus,
    listSlackWorkspaces,
    removeSlackWorkspace,
    sendSlackTestMessage,
    type SlackStatusResponse,
    type SlackWorkspaceView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

export function SlackConfigPage(_props: PluginConfigProps): JSX.Element {
    const { getAccessToken } = useAuth();
    const [status, setStatus] = useState<SlackStatusResponse | null>(null);
    const [workspaces, setWorkspaces] = useState<SlackWorkspaceView[] | null>(null);
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
                getSlackStatus(getAccessToken),
                listSlackWorkspaces(getAccessToken),
            ]);
            setStatus(s);
            setWorkspaces(w.workspaces);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getAccessToken]);

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
            const added = await addSlackWorkspace(
                botToken.trim(),
                appToken.trim(),
                trimmedController,
                getAccessToken,
            );
            // Auto-write the controller's slack-user identity to the
            // shared My Identities list so principal-admit treats
            // inbound DMs from this Slack user as Controller.
            // Skipped when the operator left it blank — they can add
            // it later via Settings → My Identities.
            if (trimmedController !== "") {
                try {
                    await addMyIdentifier("slack", trimmedController, getAccessToken);
                } catch (idErr) {
                    // Non-fatal — the workspace IS saved; the
                    // identifier just wasn't auto-registered.
                    setError(
                        `Workspace saved, but couldn't register the controller identity: ${
                            idErr instanceof Error ? idErr.message : String(idErr)
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
    }, [appToken, botToken, controllerUserId, getAccessToken, reload]);

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
                await removeSlackWorkspace(team_id, getAccessToken);
                await reload();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [getAccessToken, reload],
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
            const r = await sendSlackTestMessage(
                testTeamId,
                testChannel.trim(),
                getAccessToken,
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
    }, [getAccessToken, testChannel, testTeamId]);

    if (status === null && workspaces === null) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <Spinner animation="border" size="sm" className="me-2" />
                Loading…
            </div>
        );
    }

    const wsCount = workspaces?.length ?? 0;
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

            <Card className="mb-3">
                <Card.Body>
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Slack workspaces</h5>
                        <Badge
                            bg=""
                            className={statusBadgeClass}
                            data-testid="slack-status-badge"
                        >
                            {statusLabel}
                        </Badge>
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
                        <Alert variant="success" data-testid="slack-saved">
                            {savedNotice}
                        </Alert>
                    )}

                    {wsCount === 0 ? (
                        <div className="execlaw-muted small mb-3">
                            No workspaces yet. Add one to start receiving
                            Slack messages.
                        </div>
                    ) : (
                        <Table
                            size="sm"
                            className="mb-3"
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
                                {workspaces?.map((ws) => (
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
                                                    onClick={() => void onRemove(ws.team_id)}
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
                        </Table>
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
                            <Form.Group className="mb-2">
                                <Form.Label className="execlaw-muted small mb-1">
                                    Bot token
                                </Form.Label>
                                <Form.Control
                                    type="password"
                                    placeholder="xoxb-..."
                                    value={botToken}
                                    onChange={(e) => setBotToken(e.target.value)}
                                    data-testid="slack-bot-token-input"
                                />
                            </Form.Group>
                            <Form.Group className="mb-2">
                                <Form.Label className="execlaw-muted small mb-1">
                                    App-level token
                                </Form.Label>
                                <Form.Control
                                    type="password"
                                    placeholder="xapp-..."
                                    value={appToken}
                                    onChange={(e) => setAppToken(e.target.value)}
                                    data-testid="slack-app-token-input"
                                />
                            </Form.Group>
                            <Form.Group className="mb-3">
                                <Form.Label className="execlaw-muted small mb-1">
                                    Controller Slack user id
                                    <span className="execlaw-muted ms-1">(optional)</span>
                                </Form.Label>
                                <Form.Control
                                    type="text"
                                    placeholder="U0A5GB3BJFL"
                                    value={controllerUserId}
                                    onChange={(e) => setControllerUserId(e.target.value)}
                                    data-testid="slack-controller-user-input"
                                />
                                <Form.Text className="execlaw-muted">
                                    Your own Slack user id in this workspace
                                    (different from the bot's id). Inbound
                                    DMs from this user resolve to the
                                    controller via My Identities, skipping
                                    the cold-contact ladder. Find it: click
                                    your avatar in Slack → View profile → ⋮ →
                                    Copy member ID.
                                </Form.Text>
                            </Form.Group>
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
                </Card.Body>
            </Card>

            {testTeamId && (
                <Card className="mb-3" data-testid="slack-test-card">
                    <Card.Body>
                        <h5 className="h6 mb-2">Test message</h5>
                        <p className="execlaw-muted small mb-2">
                            Send a one-shot message to confirm the bot is
                            wired and has permission to post in the target
                            channel. The bot must be a member of the channel
                            (right-click channel → View channel details →
                            Integrations → Add apps).
                        </p>
                        <Form.Group className="mb-2">
                            <Form.Label className="execlaw-muted small mb-1">
                                Channel id (workspace: <code>{testTeamId}</code>)
                            </Form.Label>
                            <Form.Control
                                type="text"
                                placeholder="C0123456789"
                                value={testChannel}
                                onChange={(e) => setTestChannel(e.target.value)}
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") {
                                        e.preventDefault();
                                        void onTest();
                                    }
                                }}
                                autoFocus
                                data-testid="slack-test-channel-input"
                            />
                        </Form.Group>
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
                            <Alert
                                variant="success"
                                className="mt-2"
                                data-testid="slack-test-ok"
                            >
                                {testStatus.message}
                            </Alert>
                        )}
                        {testStatus.kind === "err" && (
                            <Alert
                                variant="danger"
                                className="mt-2"
                                data-testid="slack-test-err"
                            >
                                {testStatus.message}
                            </Alert>
                        )}
                    </Card.Body>
                </Card>
            )}
        </div>
    );
}
