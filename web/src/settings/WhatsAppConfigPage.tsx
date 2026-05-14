// Settings → Plugin → WhatsApp config page.
//
// Mirrors SignalConfigPage's UX one-to-one: poll status, render
// either a "Pair this install" QR-scan flow or a paired-state
// "Unlink" affordance. The plugin's admin endpoints return the
// same JSON shapes Signal uses, so the same SignalStatusResponse
// / SignalQrCodeLinkResponse types are reused via type aliases
// in api/endpoints.ts.

import { useCallback, useEffect, useState, type JSX } from "react";
import Button from "react-bootstrap/Button";
import {
    fetchWhatsAppQrCodeLink,
    getWhatsAppStatus,
    unregisterWhatsAppAccount,
    type WhatsAppStatusResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { SidecarStatusBlock } from "../components/SidecarStatusBlock";
import type { PluginConfigProps } from "./PluginConfigBase";

const POLL_INTERVAL_MS = 3_000;

export function WhatsAppConfigPage(_props: PluginConfigProps): JSX.Element {
    const auth = useAuth();
    const { getAccessToken } = auth;
    const [status, setStatus] = useState<WhatsAppStatusResponse | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await getWhatsAppStatus(getAccessToken);
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

    const onUnregister = useCallback(async () => {
        if (
            !window.confirm(
                "Unlink execlaw from your WhatsApp account?\n\n" +
                    "Inbound messages will stop reaching the agent until you re-pair.",
            )
        ) {
            return;
        }
        setBusy(true);
        try {
            await unregisterWhatsAppAccount(getAccessToken);
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [getAccessToken, refresh]);

    return (
        <div data-testid="whatsapp-config-page">
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
                        sidecarLabel="wuzapi"
                        status={status.sidecar_status}
                        rpcUrl={status.sidecar_rpc_url}
                        fetchError={status.fetch_error}
                        testidPrefix="whatsapp"
                        followupHint={
                            status.sidecar_status === "awaiting_pairing" ? (
                                <>
                                    Sidecar is up; waiting for the wuzapi
                                    user to be provisioned. The plugin
                                    auto-creates one on the first poll —
                                    usually a few seconds.
                                </>
                            ) : undefined
                        }
                    />
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

// SidecarStatusBlock moved to ../components/SidecarStatusBlock.tsx
// and shared with the Signal config page. The `awaiting_pairing`
// follow-up copy is supplied to the shared block via the
// `followupHint` prop above.

function PairingBlock({
    sidecarRunning,
    onPaired,
}: {
    sidecarRunning: boolean;
    onPaired: () => void;
}): JSX.Element {
    // 2026-05-14 — destructure `getAccessToken` so this effect
    // depends on a stable function reference, not the full `auth`
    // object whose reference flips on every auth-context recompute.
    // See `SignalConfigPage.tsx` for the full rationale — same
    // failure mode applies here (every 3-second status poll caused
    // a re-render which invalidated the displayed QR by minting a
    // fresh wuzapi pairing token).
    const { getAccessToken } = useAuth();
    const [generation, setGeneration] = useState(0);
    const [qrDataUrl, setQrDataUrl] = useState<string | null>(null);
    const [qrError, setQrError] = useState<string | null>(null);
    const [qrLoading, setQrLoading] = useState(false);

    useEffect(() => {
        if (!sidecarRunning) return;
        let cancelled = false;
        setQrLoading(true);
        setQrError(null);
        void fetchWhatsAppQrCodeLink(getAccessToken)
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
            .catch((e) => {
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
    }, [getAccessToken, generation, sidecarRunning]);

    // Refresh every 60s — WhatsApp's pairing QR rotates more
    // aggressively than Signal's; this stays inside the
    // refresh-window so the operator doesn't see "QR expired".
    useEffect(() => {
        const id = window.setInterval(() => {
            setGeneration((n) => n + 1);
        }, 60_000);
        return () => window.clearInterval(id);
    }, []);

    return (
        <div className="execlaw-card mb-3" data-testid="whatsapp-pairing-block">
            <div className="execlaw-card__title mb-2">Pair this install</div>
            <p className="execlaw-muted small mb-3">
                execlaw links to your WhatsApp account as a{" "}
                <strong>linked device</strong>, the same way WhatsApp
                Web / Desktop does. Your phone stays the primary device.
            </p>
            <ol className="small mb-3">
                <li>Open WhatsApp on your phone.</li>
                <li>
                    Go to <strong>Settings → Linked Devices → Link a
                    Device</strong> (iOS) or <strong>⋮ → Linked Devices
                    → Link a Device</strong> (Android).
                </li>
                <li>Scan the QR code below.</li>
                <li>Wait a few seconds — this page auto-detects the link.</li>
            </ol>
            {!sidecarRunning ? (
                <div
                    className="execlaw-muted small"
                    data-testid="whatsapp-pairing-waiting"
                >
                    Waiting for the wuzapi sidecar to come up before
                    generating the QR…
                </div>
            ) : qrError !== null ? (
                <div
                    className="alert alert-danger small mb-3"
                    data-testid="whatsapp-pairing-qr-error"
                >
                    <div className="fw-semibold mb-1">
                        Couldn&apos;t generate the device-link QR.
                    </div>
                    <div className="mb-2">
                        Sidecar reported: <code>{qrError}</code>
                    </div>
                    <Button
                        variant="outline-primary"
                        size="sm"
                        onClick={() => setGeneration((n) => n + 1)}
                        data-testid="whatsapp-pairing-retry"
                    >
                        Retry
                    </Button>
                </div>
            ) : (
                <div className="d-flex flex-column align-items-center gap-2">
                    {qrLoading && qrDataUrl === null ? (
                        <div
                            className="execlaw-muted small py-4"
                            data-testid="whatsapp-pairing-qr-loading"
                        >
                            Generating QR…
                        </div>
                    ) : qrDataUrl !== null ? (
                        <img
                            src={qrDataUrl}
                            alt="WhatsApp device-link QR code"
                            width={256}
                            height={256}
                            style={{
                                background: "#fff",
                                padding: "0.5rem",
                                borderRadius: "0.5rem",
                            }}
                            data-testid="whatsapp-pairing-qr"
                        />
                    ) : null}
                    <Button
                        variant="outline-primary"
                        size="sm"
                        onClick={() => {
                            setGeneration((n) => n + 1);
                            onPaired();
                        }}
                        data-testid="whatsapp-pairing-refresh"
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
    onUnregister: () => void;
}): JSX.Element {
    return (
        <div className="execlaw-card mb-3" data-testid="whatsapp-paired-block">
            <div className="execlaw-card__title mb-2">Paired</div>
            <p className="execlaw-muted small mb-3">
                execlaw is linked to the following WhatsApp account.
                Inbound messages resolve to the controller and skip the
                cold-contact ladder; outbound{" "}
                <code>whatsapp.send_message</code> calls dispatch on this
                account.
            </p>
            <ul className="list-unstyled mb-0">
                {accounts.map((number) => (
                    <li
                        key={number}
                        className="d-flex align-items-baseline gap-2 mb-2"
                        data-testid="whatsapp-paired-row"
                    >
                        <span className="execlaw-trust-badge is-known">
                            whatsapp
                        </span>
                        <code className="flex-grow-1">{number}</code>
                        <Button
                            size="sm"
                            variant="outline-danger"
                            disabled={busy}
                            onClick={onUnregister}
                            data-testid="whatsapp-paired-unlink"
                        >
                            Unlink
                        </Button>
                    </li>
                ))}
            </ul>
        </div>
    );
}

// `badgeClassForStatus` moved to the shared SidecarStatusBlock
// component (see `presentationFor` in
// `../components/SidecarStatusBlock.tsx`). The shared mapping
// also handles the `crash_looping` underscore-spelling that the
// supervisor's wire format actually emits — the local table here
// matched only `crashlooping` (no underscore) and so never fired
// for real crash-looping sidecars.
