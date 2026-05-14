// Signal plugin self-contained config panel.
//
// First plugin migrated to the dynamic-UI architecture (2026-05-14):
// this file replaces the old hardcoded `web/src/settings/SignalConfigPage.tsx`
// in the host SPA bundle. The host's `DynamicPluginPanel` loads this
// file at runtime via authenticated fetch + Blob URL + `import()`;
// the bridge passed in `props.bridge` exposes React + helpers +
// shared components so we never ship a duplicate React copy.
//
// Build it from this source:
//   npm --prefix web run build-plugin -- signal
// Watch mode for the plugin-author dev loop:
//   npm --prefix web run build-plugin -- signal --watch
//
// The produced `panel.js` ships inside `dist/signal-X.Y.Z.zip`
// alongside `plugin.toml` + `main.rhai`.

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

// React is provided by the host bridge at runtime. The build script's
// esbuild config marks `react` as external and uses the classic JSX
// transform (`jsxFactory: "React.createElement"`), so this
// module-scope const is what every JSX node resolves against.
//
// `globalThis.execlawHost` is guaranteed to be installed before the
// dynamic loader imports this file — see web/src/plugins/BridgeInstaller.tsx.
const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

interface SignalStatusResponse {
    sidecar_status: string;
    sidecar_rpc_url: string | null;
    registered_accounts: string[];
    accounts_on_disk: string[];
    fetch_error: string | null;
}

interface SignalQrCodeLinkResponse {
    data_url?: string;
    mime_type?: string;
    error?: string;
}

// --- Top-level Panel ------------------------------------------------

/** Poll cadence while the operator is on-page. 3s is responsive
 *  without hammering the supervised sidecar. */
const POLL_INTERVAL_MS = 3_000;

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge } = props;
    const { ErrorBanner, SidecarStatusBlock, Button } = bridge.components;

    const [status, setStatus] = useState<SignalStatusResponse | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await bridge.fetchJson<SignalStatusResponse>(
                "GET",
                "/api/admin/plugins/signal/status",
            );
            setStatus(r);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [bridge]);

    useEffect(() => {
        void refresh();
        const id = window.setInterval(() => {
            void refresh();
        }, POLL_INTERVAL_MS);
        return () => window.clearInterval(id);
    }, [refresh]);

    const onUnregister = useCallback(
        async (number: string) => {
            if (
                !window.confirm(
                    `Unlink execlaw from Signal account ${number}? \n\n` +
                        "Inbound messages from this number will stop reaching the agent " +
                        "until you re-pair.",
                )
            ) {
                return;
            }
            setBusy(true);
            try {
                await bridge.fetchJson<unknown>(
                    "DELETE",
                    `/api/admin/plugins/signal/unregister-account?number=${encodeURIComponent(number)}`,
                );
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [bridge, refresh],
    );

    return (
        <div data-testid="signal-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />
            {status === null ? (
                <div className="execlaw-muted small">Loading status…</div>
            ) : (
                <>
                    <SidecarStatusBlock
                        sidecarLabel="signal-cli"
                        status={status.sidecar_status}
                        rpcUrl={status.sidecar_rpc_url}
                        fetchError={status.fetch_error}
                        testidPrefix="signal"
                    />
                    {status.registered_accounts.length === 0 ? (
                        <PairingBlock
                            bridge={bridge}
                            sidecarRunning={status.sidecar_rpc_url !== null}
                            onPaired={() => void refresh()}
                            Button={Button}
                        />
                    ) : (
                        <PairedBlock
                            accounts={status.registered_accounts}
                            busy={busy}
                            onUnregister={onUnregister}
                            Button={Button}
                        />
                    )}
                </>
            )}
        </div>
    );
};

export default Panel;

// --- PairingBlock --------------------------------------------------

interface PairingBlockProps {
    bridge: PluginPanelProps["bridge"];
    sidecarRunning: boolean;
    onPaired: () => void;
    Button: ReturnType<typeof currentButton>;
}

/**
 * Tiny helper to wire up the Button type once. The bridge exposes
 * Button as a `PluginComponent<ButtonProps>`; this hoist lets the
 * sub-component prop typing be derived from the bridge instance
 * rather than imported separately.
 */
function currentButton() {
    return globalThis.execlawHost!.components.Button;
}

function PairingBlock({
    bridge,
    sidecarRunning,
    onPaired,
    Button,
}: PairingBlockProps) {
    const [generation, setGeneration] = useState(0);
    const [qrDataUrl, setQrDataUrl] = useState<string | null>(null);
    const [qrError, setQrError] = useState<string | null>(null);
    const [qrLoading, setQrLoading] = useState(false);

    // QR fetch effect. Depends on:
    //   * `generation` — the deliberate 60s refresh + the Retry
    //     button bump it.
    //   * `sidecarRunning` — re-fetch when the sidecar comes up.
    //   * `bridge` is a stable reference (the bridge global doesn't
    //     get reinstalled; same instance across renders); listing
    //     it here keeps eslint happy without re-firing the effect.
    useEffect(() => {
        if (!sidecarRunning) return;
        let cancelled = false;
        setQrLoading(true);
        setQrError(null);
        void bridge
            .fetchJson<SignalQrCodeLinkResponse>(
                "GET",
                "/api/admin/plugins/signal/qrcodelink?device_name=execlaw",
            )
            .then((r) => {
                if (cancelled) return;
                if (r.error) {
                    setQrError(r.error);
                    setQrDataUrl(null);
                    return;
                }
                if (r.data_url) {
                    setQrDataUrl(r.data_url);
                    setQrError(null);
                } else {
                    setQrError("plugin returned neither data_url nor error");
                    setQrDataUrl(null);
                }
            })
            .catch((e: unknown) => {
                if (cancelled) return;
                setQrError(e instanceof Error ? e.message : String(e));
                setQrDataUrl(null);
            })
            .finally(() => {
                if (!cancelled) setQrLoading(false);
            });
        return () => {
            cancelled = true;
        };
    }, [bridge, generation, sidecarRunning]);

    // Auto-refresh the QR every 60s so signal-cli's pairing nonce
    // doesn't time out behind a stale image.
    useEffect(() => {
        const id = window.setInterval(() => {
            setGeneration((n) => n + 1);
        }, 60_000);
        return () => window.clearInterval(id);
    }, []);

    const looksLikeConnectivityIssue =
        qrError !== null &&
        /no data to encode|Connection closed|certificate|resolve host/i.test(
            qrError,
        );

    return (
        <div className="execlaw-card mb-3" data-testid="signal-pairing-block">
            <div className="execlaw-card__title mb-2">Pair this install</div>
            <p className="execlaw-muted small mb-3">
                execlaw links to your Signal account as a{" "}
                <strong>secondary device</strong>, the same way Signal Desktop
                does. Your phone stays the primary device — you don&apos;t give
                up your number to the sidecar.
            </p>
            <ol className="small mb-3">
                <li>Open Signal on your phone.</li>
                <li>
                    Go to{" "}
                    <strong>
                        Settings → Linked devices → Link new device
                    </strong>
                    .
                </li>
                <li>Scan the QR code below.</li>
                <li>
                    Name this device when prompted (e.g. <code>execlaw</code>)
                    and confirm.
                </li>
            </ol>
            {!sidecarRunning ? (
                <div
                    className="execlaw-muted small"
                    data-testid="signal-pairing-waiting"
                >
                    Waiting for the sidecar to come up before generating the
                    QR…
                </div>
            ) : qrError !== null ? (
                <div
                    className="alert alert-danger small mb-3"
                    data-testid="signal-pairing-qr-error"
                >
                    <div className="fw-semibold mb-1">
                        Couldn&apos;t generate the device-link QR.
                    </div>
                    <div className="mb-2">
                        Sidecar reported: <code>{qrError}</code>
                    </div>
                    {looksLikeConnectivityIssue && (
                        <div className="mb-2">
                            This usually means the signal-cli sidecar
                            can&apos;t reach Signal&apos;s servers. Common
                            causes:
                            <ul className="mb-1 ps-3">
                                <li>
                                    Antivirus / security software on the host
                                    is doing HTTPS scanning (signal-cli pins
                                    Signal&apos;s certs and rejects any
                                    intercepted TLS).
                                </li>
                                <li>
                                    Corporate VPN, transparent proxy, or DNS
                                    filter is blocking{" "}
                                    <code>chat.signal.org</code> /{" "}
                                    <code>
                                        textsecure-service.whispersystems.org
                                    </code>
                                    .
                                </li>
                                <li>
                                    The network the host is on doesn&apos;t
                                    allow outbound traffic to Signal.
                                </li>
                            </ul>
                        </div>
                    )}
                    <Button
                        variant="outline-primary"
                        size="sm"
                        onClick={() => setGeneration((n: number) => n + 1)}
                        data-testid="signal-pairing-retry"
                    >
                        Retry
                    </Button>
                </div>
            ) : (
                <div className="d-flex flex-column align-items-center gap-2">
                    {qrLoading && qrDataUrl === null ? (
                        <div
                            className="execlaw-muted small py-4"
                            data-testid="signal-pairing-qr-loading"
                        >
                            Generating QR…
                        </div>
                    ) : qrDataUrl !== null ? (
                        <img
                            src={qrDataUrl}
                            alt="Signal device-link QR code"
                            width={256}
                            height={256}
                            style={{
                                background: "#fff",
                                padding: "0.5rem",
                                borderRadius: "0.5rem",
                            }}
                            data-testid="signal-pairing-qr"
                        />
                    ) : null}
                    <Button
                        variant="outline-primary"
                        size="sm"
                        onClick={() => {
                            setGeneration((n: number) => n + 1);
                            onPaired();
                        }}
                        data-testid="signal-pairing-refresh"
                    >
                        Regenerate QR
                    </Button>
                    <span
                        className="execlaw-muted small"
                        style={{ maxWidth: "32rem", textAlign: "center" }}
                    >
                        The QR auto-refreshes every minute. After scanning,
                        this page detects the new pairing within a few seconds
                        — no need to reload.
                    </span>
                </div>
            )}
        </div>
    );
}

// --- PairedBlock ---------------------------------------------------

interface PairedBlockProps {
    accounts: string[];
    busy: boolean;
    onUnregister: (number: string) => void;
    Button: ReturnType<typeof currentButton>;
}

function PairedBlock({
    accounts,
    busy,
    onUnregister,
    Button,
}: PairedBlockProps) {
    return (
        <div className="execlaw-card mb-3" data-testid="signal-paired-block">
            <div className="execlaw-card__title mb-2">Paired</div>
            <p className="execlaw-muted small mb-3">
                execlaw is linked to{" "}
                {accounts.length === 1
                    ? "the following Signal account"
                    : "the following Signal accounts"}
                . Inbound messages from{" "}
                {accounts.length === 1 ? "it" : "these numbers"} resolve to the
                controller and skip the cold-contact ladder; outbound{" "}
                <code>signal.send_message</code> calls dispatch on{" "}
                {accounts.length === 1 ? "this account" : "these accounts"}.
            </p>
            <ul className="list-unstyled mb-0">
                {accounts.map((number: string) => (
                    <li
                        key={number}
                        className="d-flex align-items-baseline gap-2 mb-2"
                        data-testid="signal-paired-row"
                    >
                        <span className="execlaw-trust-badge is-known">
                            signal
                        </span>
                        <code className="flex-grow-1">{number}</code>
                        <Button
                            size="sm"
                            variant="outline-danger"
                            disabled={busy}
                            onClick={() => onUnregister(number)}
                            data-testid="signal-paired-unlink"
                        >
                            Unlink
                        </Button>
                    </li>
                ))}
            </ul>
        </div>
    );
}
