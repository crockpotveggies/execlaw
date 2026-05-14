// Discord plugin self-contained config panel.
//
// Migrated from `web/src/settings/DiscordConfigPage.tsx` (2026-05-14).
// Build: node scripts/build-plugin-ui.mjs discord

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface DiscordConfigResponse {
    bot_token_masked: string;
    configured: boolean;
    bot_user_id?: string | null;
    bot_username?: string | null;
}

interface DiscordStatusResponse {
    sidecar_status: string;
    sidecar_rpc_url: string | null;
    registered_accounts: unknown[];
    accounts_on_disk: unknown[];
    fetch_error: string | null;
    configured?: boolean;
    bot_user_id?: string | null;
    bot_username?: string | null;
    guilds_known?: number;
    token_masked?: string;
}

interface DiscordTestResponse {
    ok?: boolean;
    message_id?: string;
    error?: string;
}

interface DiscordSetConfigResponse {
    ok: boolean;
    bot_user_id?: string;
    bot_username?: string;
}

const Panel: PluginPanelComponent = (_props: PluginPanelProps) => {
    const { bridge } = _props;
    const { ErrorBanner, Button } = bridge.components;

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
                bridge.fetchJson<DiscordConfigResponse>(
                    "GET",
                    "/api/admin/plugins/discord/config",
                ),
                bridge.fetchJson<DiscordStatusResponse>(
                    "GET",
                    "/api/admin/plugins/discord/status",
                ),
            ]);
            setConfig(c);
            setStatus(s);
        } catch (e) {
            setError(e instanceof Error ? e.message : "couldn't load config");
        } finally {
            setLoading(false);
        }
    }, [bridge]);

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
            const r = await bridge.fetchJson<DiscordSetConfigResponse>(
                "POST",
                "/api/admin/plugins/discord/config",
                { bot_token: trimmed },
            );
            const who = r.bot_username
                ? `${r.bot_username} (id ${r.bot_user_id ?? "?"})`
                : "the bot";
            setSavedNotice(
                `Saved. Discord accepted the token for ${who}; the plugin tore down its old gateway connection and is reconnecting now — guild count below will populate as GUILD_CREATE dispatches arrive.`,
            );
            setBotToken("");
            await reload();
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [botToken, bridge, reload]);

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
            const r = await bridge.fetchJson<DiscordTestResponse>(
                "POST",
                "/api/admin/plugins/discord/test",
                { channel_id: channelId },
            );
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
    }, [bridge, reload, testChannel]);

    if (loading) {
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

            <div className="card mb-3">
                <div className="card-body">
                    <div className="d-flex align-items-center mb-2 gap-2">
                        <h5 className="h6 mb-0">Bot token</h5>
                        {configured ? (
                            <span
                                className="badge bg-success"
                                data-testid="discord-status"
                            >
                                Configured
                            </span>
                        ) : (
                            <span
                                className="badge bg-warning text-dark"
                                data-testid="discord-status"
                            >
                                Unconfigured
                            </span>
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
                        and copy the resulting token — it&apos;s shown exactly once.
                    </p>
                    <div
                        className="alert alert-warning small py-2"
                        data-testid="discord-intent-warning"
                    >
                        <strong>Privileged intent required.</strong> On the
                        same page, scroll to{" "}
                        <em>Privileged Gateway Intents</em> and enable{" "}
                        <code>MESSAGE CONTENT INTENT</code>. Without it the
                        plugin will connect, but every inbound message will
                        arrive with an empty <code>content</code> field and the
                        agent won&apos;t see what users typed.
                    </div>

                    {savedNotice && (
                        <div className="alert alert-success" data-testid="discord-saved">
                            {savedNotice}
                        </div>
                    )}

                    <div className="mb-3">
                        <label className="form-label execlaw-muted small mb-1">
                            Bot token
                            {configured && config?.bot_token_masked && (
                                <span className="ms-2 execlaw-muted">
                                    (currently:{" "}
                                    <code>{config.bot_token_masked}</code>)
                                </span>
                            )}
                        </label>
                        <input
                            type="password"
                            className="form-control"
                            placeholder="paste the bot token from the developer portal"
                            value={botToken}
                            onChange={(e: { target: { value: string } }) =>
                                setBotToken(e.target.value)
                            }
                            data-testid="discord-token-input"
                            autoComplete="off"
                        />
                        <div className="form-text execlaw-muted">
                            Sent as <code>Authorization: Bot &lt;token&gt;</code>{" "}
                            against the Discord REST API. Validated via{" "}
                            <code>GET /users/@me</code> before save —
                            invalid tokens are rejected without writing to
                            the vault.
                        </div>
                    </div>

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
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
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
                                <span
                                    className={
                                        "badge " +
                                        (guildsKnown > 0
                                            ? "bg-info"
                                            : "bg-secondary")
                                    }
                                    data-testid="discord-guild-count"
                                >
                                    {guildsKnown}
                                </span>
                            </span>
                        </div>
                    )}
                </div>
            </div>

            <div className="card mb-3">
                <div className="card-body">
                    <h5 className="h6 mb-2">Test message</h5>
                    <p className="execlaw-muted small mb-2">
                        Sends a one-shot message to a Discord channel so you
                        can confirm wiring end-to-end. The send goes via{" "}
                        <code>POST /channels/{`{id}`}/messages</code>, not the
                        gateway — useful even before the gateway WebSocket
                        comes up.
                    </p>
                    <div className="mb-2">
                        <label className="form-label execlaw-muted small mb-1">
                            Channel id
                        </label>
                        <input
                            type="text"
                            className="form-control"
                            placeholder="e.g. 1234567890123456789"
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
                            data-testid="discord-test-channel-input"
                        />
                        <div className="form-text execlaw-muted">
                            Enable Developer Mode in Discord (User Settings →
                            Advanced) to expose <em>Copy Channel ID</em> on
                            the right-click menu.
                        </div>
                    </div>
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
                        <div
                            className="alert alert-success mt-2"
                            data-testid="discord-test-ok"
                        >
                            {testStatus.message}
                        </div>
                    )}
                    {testStatus.kind === "err" && (
                        <div
                            className="alert alert-danger mt-2"
                            data-testid="discord-test-err"
                        >
                            {testStatus.message}
                        </div>
                    )}
                </div>
            </div>
        </div>
    );
};

export default Panel;
