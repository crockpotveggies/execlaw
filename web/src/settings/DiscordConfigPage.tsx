// Settings → Plugins → Discord.
//
// The operator creates a bot in the Discord Developer Portal,
// enables the `MESSAGE_CONTENT` privileged gateway intent (Bot →
// Privileged Gateway Intents → MESSAGE CONTENT INTENT), copies the
// bot token, and pastes it here. On save the plugin validates the
// token against `GET /users/@me`, persists it to the per-plugin
// vault, then hot-reloads the gateway connection.
//
// Three affordances (mirrors SmsSocketConfigPage shape):
//   * Save form — single bot_token input. Server masks the token
//     on read so refreshing doesn't leak the original.
//   * Status card — surfaces the bot identity (id + username) and
//     how many guilds the gateway has reported (via GUILD_CREATE).
//   * Test send — invites the operator to paste a channel id and
//     queue a one-shot message to confirm end-to-end wiring.

import { useCallback, useEffect, useState, type JSX } from "react";
import { Alert, Badge, Button, Card, Form, Spinner } from "react-bootstrap";
import {
    getDiscordConfig,
    getDiscordStatus,
    setDiscordConfig,
    testDiscordMessage,
    type DiscordConfigResponse,
    type DiscordStatusResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

export function DiscordConfigPage(_props: PluginConfigProps): JSX.Element {
    const { getAccessToken } = useAuth();
    const [config, setConfig] = useState<DiscordConfigResponse | null>(null);
    const [status, setStatus] = useState<DiscordStatusResponse | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [botToken, setBotToken] = useState("");
    const [savedNotice, setSavedNotice] = useState<string | null>(null);
    const [testChannel, setTestChannel] = useState("");
    const [testStatus, setTestStatus] = useState<
        | { kind: "idle" }
        | { kind: "ok"; message: string }
        | { kind: "err"; message: string }
    >({ kind: "idle" });

    const reload = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const [c, s] = await Promise.all([
                getDiscordConfig(getAccessToken),
                getDiscordStatus(getAccessToken),
            ]);
            setConfig(c);
            setStatus(s);
        } catch (e) {
            setError(e instanceof Error ? e.message : "couldn't load config");
        } finally {
            setLoading(false);
        }
    }, [getAccessToken]);

    useEffect(() => {
        void reload();
    }, [reload]);

    const onSave = useCallback(async () => {
        const trimmed = botToken.trim();
        if (trimmed === "") {
            setError("Bot token is required.");
            return;
        }
        setBusy(true);
        setError(null);
        setSavedNotice(null);
        setTestStatus({ kind: "idle" });
        try {
            const r = await setDiscordConfig(trimmed, getAccessToken);
            const who = r.bot_username
                ? `${r.bot_username} (id ${r.bot_user_id ?? "?"})`
                : "the bot";
            setSavedNotice(
                `Saved. Discord accepted the token for ${who}; the plugin tore down its old gateway connection and is reconnecting now — guild count below will populate as GUILD_CREATE dispatches arrive.`,
            );
            // Don't keep the cleartext token in the input after a
            // successful save — server-side masking is the source
            // of truth from this point on.
            setBotToken("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [botToken, getAccessToken, reload]);

    const onTest = useCallback(async () => {
        const channelId = testChannel.trim();
        if (channelId === "") {
            setTestStatus({
                kind: "err",
                message:
                    "Channel id required. Right-click a Discord channel and pick Copy Channel ID (you need Developer Mode enabled in Discord settings to see that option).",
            });
            return;
        }
        setBusy(true);
        setTestStatus({ kind: "idle" });
        setError(null);
        try {
            const r = await testDiscordMessage(channelId, getAccessToken);
            if (r.ok === false) {
                setTestStatus({
                    kind: "err",
                    message:
                        r.error ??
                        "Discord rejected the test send. Confirm the channel id is correct and the bot has Send Messages permission.",
                });
            } else {
                setTestStatus({
                    kind: "ok",
                    message: `Sent. Discord message id: ${r.message_id ?? "(none returned)"}.`,
                });
            }
            await reload();
        } catch (e) {
            setTestStatus({
                kind: "err",
                message: e instanceof Error ? e.message : String(e),
            });
        } finally {
            setBusy(false);
        }
    }, [getAccessToken, reload, testChannel]);

    if (loading) {
        return (
            <div className="d-flex align-items-center execlaw-muted">
                <Spinner animation="border" size="sm" className="me-2" />
                Loading…
            </div>
        );
    }

    const configured = config?.configured ?? false;
    const botUsername = config?.bot_username ?? status?.bot_username ?? null;
    const botUserId = config?.bot_user_id ?? status?.bot_user_id ?? null;
    const guildsKnown = status?.guilds_known ?? 0;

    return (
        <div data-testid="discord-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <Card className="mb-3">
                <Card.Body>
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Bot token</h5>
                        {configured ? (
                            <Badge bg="success" data-testid="discord-status">
                                Configured
                            </Badge>
                        ) : (
                            <Badge
                                bg="warning"
                                text="dark"
                                data-testid="discord-status"
                            >
                                Unconfigured
                            </Badge>
                        )}
                    </div>
                    <p className="execlaw-muted small mb-2">
                        Create an application in the{" "}
                        <a
                            href="https://discord.com/developers/applications"
                            target="_blank"
                            rel="noopener noreferrer"
                        >
                            Discord Developer Portal
                        </a>
                        , go to <strong>Bot</strong>, click{" "}
                        <em>Reset Token</em> (or <em>Add Bot</em> on a new app)
                        and copy the resulting token — it's shown exactly once.
                    </p>
                    <Alert
                        variant="warning"
                        className="small py-2"
                        data-testid="discord-intent-warning"
                    >
                        <strong>Privileged intent required.</strong> On the
                        same page, scroll to{" "}
                        <em>Privileged Gateway Intents</em> and enable{" "}
                        <code>MESSAGE CONTENT INTENT</code>. Without it the
                        plugin will connect, but every inbound message will
                        arrive with an empty <code>content</code> field and the
                        agent won't see what users typed.
                    </Alert>

                    {savedNotice && (
                        <Alert variant="success" data-testid="discord-saved">
                            {savedNotice}
                        </Alert>
                    )}

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Bot token
                            {configured && config?.bot_token_masked && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config.bot_token_masked}</code>)
                                </span>
                            )}
                        </Form.Label>
                        <Form.Control
                            type="password"
                            placeholder="paste the bot token from the developer portal"
                            value={botToken}
                            onChange={(e) => setBotToken(e.target.value)}
                            data-testid="discord-token-input"
                            autoComplete="off"
                        />
                        <Form.Text className="execlaw-muted">
                            Sent as <code>Authorization: Bot &lt;token&gt;</code>{" "}
                            against the Discord REST API. Validated via{" "}
                            <code>GET /users/@me</code> before save —
                            invalid tokens are rejected without writing to
                            the vault.
                        </Form.Text>
                    </Form.Group>

                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={busy}
                            data-testid="discord-save"
                        >
                            Save
                        </Button>
                    </div>
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Gateway status</h5>
                    {!configured ? (
                        <p className="execlaw-muted small mb-0">
                            Configure the bot token above first; gateway
                            identity + guild list populate after the next
                            <code> READY </code>and<code> GUILD_CREATE </code>
                            dispatches.
                        </p>
                    ) : (
                        <div
                            className="d-flex flex-wrap gap-3 align-items-center"
                            data-testid="discord-gateway-status"
                        >
                            <span>
                                Bot identity:{" "}
                                {botUsername ? (
                                    <>
                                        <code>{botUsername}</code>{" "}
                                        <span className="execlaw-muted small">
                                            (id {botUserId})
                                        </span>
                                    </>
                                ) : (
                                    <span className="execlaw-muted small">
                                        not yet observed — waiting for READY
                                    </span>
                                )}
                            </span>
                            <span>
                                Guilds known:{" "}
                                <Badge
                                    bg={guildsKnown > 0 ? "info" : "secondary"}
                                    data-testid="discord-guild-count"
                                >
                                    {guildsKnown}
                                </Badge>
                            </span>
                        </div>
                    )}
                </Card.Body>
            </Card>

            <Card className="mb-3">
                <Card.Body>
                    <h5 className="h6 mb-2">Test message</h5>
                    <p className="execlaw-muted small mb-2">
                        Sends a one-shot message to a Discord channel so you
                        can confirm wiring end-to-end. The send goes via{" "}
                        <code>POST /channels/{`{id}`}/messages</code>, not the
                        gateway — useful even before the gateway WebSocket
                        comes up.
                    </p>
                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Channel id
                        </Form.Label>
                        <Form.Control
                            type="text"
                            placeholder="e.g. 1234567890123456789"
                            value={testChannel}
                            onChange={(e) => setTestChannel(e.target.value)}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                    e.preventDefault();
                                    void onTest();
                                }
                            }}
                            data-testid="discord-test-channel-input"
                        />
                        <Form.Text className="execlaw-muted">
                            Enable Developer Mode in Discord (User Settings →
                            Advanced) to expose <em>Copy Channel ID</em> on
                            the right-click menu.
                        </Form.Text>
                    </Form.Group>
                    <Button
                        size="sm"
                        variant="outline-secondary"
                        onClick={() => void onTest()}
                        disabled={busy || !configured}
                        data-testid="discord-test"
                    >
                        Send test message
                    </Button>
                    {testStatus.kind === "ok" && (
                        <Alert
                            variant="success"
                            className="mt-2"
                            data-testid="discord-test-ok"
                        >
                            {testStatus.message}
                        </Alert>
                    )}
                    {testStatus.kind === "err" && (
                        <Alert
                            variant="danger"
                            className="mt-2"
                            data-testid="discord-test-err"
                        >
                            {testStatus.message}
                        </Alert>
                    )}
                </Card.Body>
            </Card>
        </div>
    );
}
