// Settings → Sidecars (Phase 2b: companion-container supervisor).
//
// Each row is a supervised sidecar — a plugin-managed companion
// container the sidecar supervisor owns. The system is generic:
// transport bridges (signal-cli, WhatsApp, ...), helper workers
// (ffmpeg, OCR, ...), anything a plugin author wants the control
// plane to spawn + healthcheck. Each sidecar is keyed by its
// globally-unique `name`.
//
// Operator action:
//   * Reset → clear a parked CrashLooping slot's restart counter.
//             The next reconcile re-attempts the spawn. Only useful
//             AFTER fixing the underlying issue (image edit,
//             secrets re-mount).
//
// Polled at 5s (mirrors RunnersPage cadence). The supervisor's
// SidecarStatusChanged WS event would let us go live, but polling
// is enough for a settings-page surface and avoids one more WS
// subscription.
//
// See docs/sidecar-supervisor-design.md for the full lifecycle policy.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import { ApiError } from "../api/client";
import {
    listSidecars,
    resetSidecarAttempts,
    type SidecarView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

const POLL_INTERVAL_MS = 5_000;

/// Map the supervisor's lowercase status to a visual class + label.
/// `is-known` = green-ish, `is-pending` = yellow, `is-blocked` = red.
function statusBadge(status: string): { className: string; label: string } {
    switch (status) {
        case "healthy":
            return { className: "is-known", label: "healthy" };
        case "starting":
        case "pulling":
            return { className: "is-pending", label: status };
        case "stopped":
        case "not_found":
            return { className: "is-pending", label: status.replace("_", " ") };
        case "crash_looping":
            return { className: "is-blocked", label: "crash looping" };
        default:
            return { className: "is-known", label: status };
    }
}

export function SidecarsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [sidecars, setSidecars] = useState<SidecarView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyName, setBusyName] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listSidecars(getToken);
            setSidecars(r.sidecars);
            setError(null);
        } catch (e) {
            // 503 = supervisor not configured (no Docker, no plugins
            // declaring [services.sidecar]). Show a friendly hint
            // rather than a scary stack trace.
            if (e instanceof ApiError && e.status === 503) {
                setSidecars([]);
                setError(
                    "Sidecar supervisor is not running. This usually " +
                        "means no installed plugin declares a " +
                        "[services.sidecar] entry, or the control plane " +
                        "doesn't have a Docker connection. Install a " +
                        "transport plugin (Signal, WhatsApp, ...) to see " +
                        "live sidecars on this page.",
                );
                return;
            }
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
        const handle = setInterval(() => void refresh(), POLL_INTERVAL_MS);
        return () => clearInterval(handle);
    }, [refresh]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    const onReset = useCallback(
        async (s: SidecarView) => {
            if (
                !confirm(
                    `Reset restart counter for the "${s.name}" sidecar? This clears the parked CrashLooping state and lets the supervisor re-attempt the spawn on the next reconcile. Only useful AFTER you've fixed the underlying issue.`,
                )
            )
                return;
            setBusyName(s.name);
            try {
                await resetSidecarAttempts(s.name, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyName(null);
            }
        },
        [getToken, refresh],
    );

    return (
        <div data-testid="settings-sidecars">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Sidecars</h3>
                <Button
                    size="sm"
                    variant="outline-secondary"
                    onClick={() => void refresh()}
                    data-testid="sidecars-refresh"
                >
                    <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                    Refresh
                </Button>
            </div>

            <p className="execlaw-muted small mb-3">
                Plugins can declare companion containers via{" "}
                <code>[services.sidecar]</code> in their manifest. The
                control plane spawns + healthchecks them automatically;
                this page surfaces their live state. Use{" "}
                <strong>Reset</strong> after fixing a wedged sidecar to
                clear its CrashLooping park and trigger an immediate
                respawn attempt.
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can reset sidecars.
                </div>
            )}

            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            {sidecars === null ? (
                <div className="execlaw-muted small">Loading sidecars…</div>
            ) : sidecars.length === 0 ? (
                <div className="execlaw-muted small">
                    No sidecars registered. Install a plugin that declares
                    a <code>[services.sidecar]</code> entry to see live
                    rows here.
                </div>
            ) : (
                sidecars.map((s) => {
                    const b = statusBadge(s.status);
                    const showReset =
                        canMutate && s.status === "crash_looping";
                    return (
                        <div
                            className="execlaw-card"
                            key={s.name}
                            data-testid="sidecar-row"
                            data-name={s.name}
                        >
                            <div className="d-flex align-items-center gap-2 mb-1">
                                <span className="execlaw-card__title flex-grow-1">
                                    <code>{s.name}</code>
                                    <span
                                        className={`execlaw-trust-badge ms-2 ${b.className}`}
                                        data-status={s.status}
                                    >
                                        {b.label}
                                    </span>
                                    {s.restart_attempts > 0 && (
                                        <span
                                            className="execlaw-trust-badge ms-2 is-pending"
                                            data-testid="sidecar-restart-attempts"
                                        >
                                            restart {s.restart_attempts}/5
                                        </span>
                                    )}
                                </span>
                                {showReset && (
                                    <Button
                                        size="sm"
                                        variant="outline-secondary"
                                        disabled={busyName === s.name}
                                        onClick={() => void onReset(s)}
                                        data-testid="sidecar-reset"
                                    >
                                        Reset
                                    </Button>
                                )}
                            </div>
                            <div className="execlaw-muted small">
                                from plugin <code>{s.plugin_id}</code>
                                {s.rpc_url && (
                                    <>
                                        {" · RPC "}
                                        <code>{s.rpc_url}</code>
                                    </>
                                )}
                            </div>
                        </div>
                    );
                })
            )}
        </div>
    );
}
