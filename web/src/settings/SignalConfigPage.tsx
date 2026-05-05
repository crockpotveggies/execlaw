// Settings → Plugin → Signal config page (Phase 8).
//
// Operator-facing pairing UX. Two states:
//
//   * **Not paired** — sidecar is running but `/v1/accounts` is
//     empty. We surface a "Link this execlaw install as a secondary
//     device" workflow: generate a QR code from the supervised
//     sidecar's `GET /v1/qrcodelink`, display it as an inline
//     <img>. The operator opens Signal on their phone →
//     Settings → Linked devices → Link new device → scans the QR.
//     execlaw becomes a linked device of the operator's existing
//     Signal account; the sidecar starts receiving messages
//     immediately. Better than the SMS-verification flow because
//     it doesn't require the operator to give up their primary
//     Signal phone number to a sidecar.
//
//   * **Paired** — `/v1/accounts` returned at least one E.164
//     number. We render the number with an "Unlink" affordance
//     (POST `DELETE /api/admin/signal/accounts/{number}`); the
//     operator confirms before the unregister fires.
//
// The sidecar status chip shows live supervisor state so a stuck
// "starting" sidecar surfaces here instead of leaving the operator
// confused why the QR endpoint 503s.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Button from "react-bootstrap/Button";
import {
    getSignalStatus,
    signalQrCodeLinkUrl,
    unregisterSignalAccount,
    type SignalStatusResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import type { PluginConfigProps } from "./PluginConfigBase";

/// Poll cadence while the operator is on-page. The QR-code scan
/// flow needs the SPA to notice the new account binding within a
/// few seconds; 3s is responsive without hammering the sidecar.
const POLL_INTERVAL_MS = 3_000;

export function SignalConfigPage(_props: PluginConfigProps): JSX.Element {
    const auth = useAuth();
    const { getAccessToken } = auth;
    const [status, setStatus] = useState<SignalStatusResponse | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await getSignalStatus(getAccessToken);
            setStatus(r);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getAccessToken]);

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
                await unregisterSignalAccount(number, getAccessToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [getAccessToken, refresh],
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
                    <SidecarStatusBlock status={status} />
                    {status.registered_accounts.length === 0 ? (
                        <PairingBlock
                            sidecarRunning={status.sidecar_rpc_url !== null}
                            onPaired={refresh}
                        />
                    ) : (
                        <PairedBlock
                            accounts={status.registered_accounts}
                            busy={busy}
                            onUnregister={onUnregister}
                        />
                    )}
                </>
            )}
        </div>
    );
}

function SidecarStatusBlock({
    status,
}: {
    status: SignalStatusResponse;
}): JSX.Element {
    const chipClass = badgeClassForStatus(status.sidecar_status);
    return (
        <div className="execlaw-card mb-3" data-testid="signal-sidecar-block">
            <div className="execlaw-card__title mb-2">signal-cli sidecar</div>
            <div className="d-flex align-items-center gap-2 small mb-2">
                <span
                    className={`badge ${chipClass}`}
                    data-testid="signal-sidecar-status"
                >
                    {status.sidecar_status}
                </span>
                {status.sidecar_rpc_url && (
                    <code
                        className="execlaw-muted"
                        title="Loopback URL the supervisor published"
                    >
                        {status.sidecar_rpc_url}
                    </code>
                )}
            </div>
            {status.fetch_error && (
                <div
                    className="execlaw-muted small"
                    data-testid="signal-sidecar-fetch-error"
                >
                    Account fetch failed: {status.fetch_error}
                </div>
            )}
            {status.sidecar_status !== "healthy" && (
                <div className="execlaw-muted small">
                    Sidecar isn&apos;t healthy yet. Pairing waits until the
                    supervisor brings it up — see{" "}
                    <strong>Settings → Sidecars</strong> for restart attempts
                    and recent errors.
                </div>
            )}
        </div>
    );
}

function PairingBlock({
    sidecarRunning,
    onPaired,
}: {
    sidecarRunning: boolean;
    onPaired: () => void;
}): JSX.Element {
    const auth = useAuth();
    const [generation, setGeneration] = useState(0);
    const sawScanRef = useRef(false);
    // Refresh the QR src by bumping a generation suffix every
    // ~60s — signal-cli's pairing nonce expires after a window
    // and serving a stale image past that point would silently
    // fail. The token-attached URL means the <img> reloads
    // through the auth-aware SPA fetch.
    useEffect(() => {
        const id = window.setInterval(() => {
            setGeneration((n) => n + 1);
        }, 60_000);
        return () => window.clearInterval(id);
    }, []);

    const tok = auth.getAccessToken();
    const qrUrl = useMemo(
        () => `${signalQrCodeLinkUrl(tok, "execlaw")}&_g=${generation}`,
        [tok, generation],
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
                    Go to <strong>Settings → Linked devices → Link new
                    device</strong>.
                </li>
                <li>Scan the QR code below.</li>
                <li>
                    Name this device when prompted (e.g.{" "}
                    <code>execlaw</code>) and confirm.
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
            ) : (
                <div className="d-flex flex-column align-items-center gap-2">
                    <img
                        src={qrUrl}
                        alt="Signal device-link QR code"
                        width={256}
                        height={256}
                        style={{
                            background: "#fff",
                            padding: "0.5rem",
                            borderRadius: "0.5rem",
                        }}
                        onLoad={() => {
                            sawScanRef.current = true;
                        }}
                        data-testid="signal-pairing-qr"
                    />
                    <Button
                        variant="outline-primary"
                        size="sm"
                        onClick={() => {
                            setGeneration((n) => n + 1);
                            // Manually nudge the polling parent so a
                            // freshly-scanned QR surfaces fast.
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

function PairedBlock({
    accounts,
    busy,
    onUnregister,
}: {
    accounts: string[];
    busy: boolean;
    onUnregister: (number: string) => void;
}): JSX.Element {
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
                {accounts.map((number) => (
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

function badgeClassForStatus(status: string): string {
    switch (status) {
        case "healthy":
            return "bg-success";
        case "starting":
        case "pulling":
            return "bg-info";
        case "crashlooping":
            return "bg-danger";
        case "stopped":
        case "unwired":
            return "bg-secondary";
        default:
            return "bg-secondary";
    }
}
