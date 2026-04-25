// Settings → Runners (Phase 8.5 view-only + restart).
//
// Runners are managed automatically by the control plane — one per
// conversation, hot for ~10 minutes idle, except the Controller's
// runner which stays hot indefinitely. There's no "create" or
// "delete" affordance here; the operator's only mutating action is
// **Restart**, which forces a fresh hydration on the next turn
// (used when a runner is wedged or the operator wants to drop a
// stale in-memory state).
//
// See docs/runner-design.md for the full lifecycle policy.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import {
    listRunners,
    restartRunner,
    type RunnerView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

const POLL_INTERVAL_MS = 5_000;

function fmtCountdown(secs: number | null): string {
    if (secs === null) return "—";
    if (secs <= 0) return "0s";
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return m > 0 ? `${m}m${s.toString().padStart(2, "0")}s` : `${s}s`;
}

function fmtRelative(seconds_epoch: number, now: number): string {
    const delta = Math.max(0, now - seconds_epoch);
    if (delta < 60) return `${delta}s ago`;
    if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
    if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
    return `${Math.floor(delta / 86400)}d ago`;
}

export function RunnersPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [runners, setRunners] = useState<RunnerView[] | null>(null);
    const [idleTtlSecs, setIdleTtlSecs] = useState<number | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    const [now, setNow] = useState<number>(() =>
        Math.floor(Date.now() / 1000),
    );

    const refresh = useCallback(async () => {
        try {
            const r = await listRunners(getToken);
            setRunners(r.runners);
            setIdleTtlSecs(r.idle_ttl_secs);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    // Initial load + 5s polling so countdowns + status update without
    // requiring a manual refresh.
    useEffect(() => {
        void refresh();
        const handle = setInterval(() => {
            void refresh();
            setNow(Math.floor(Date.now() / 1000));
        }, POLL_INTERVAL_MS);
        return () => clearInterval(handle);
    }, [refresh]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    const onRestart = useCallback(
        async (r: RunnerView) => {
            if (
                !confirm(
                    `Restart the runner for "${r.principal_label ?? r.conversation_id}"? Any in-flight tool call is dropped; the next turn will rehydrate from the event log.`,
                )
            )
                return;
            setBusyId(r.conversation_id);
            try {
                await restartRunner(r.conversation_id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    return (
        <div data-testid="settings-runners">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Runners</h3>
                <Button
                    size="sm"
                    variant="outline-secondary"
                    onClick={() => void refresh()}
                    data-testid="runners-refresh"
                >
                    <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                    Refresh
                </Button>
            </div>

            <p className="execlaw-muted small mb-3">
                The control plane manages one runner per conversation
                automatically. The controller's runner is always hot;
                every other runner is reaped after{" "}
                <strong>
                    {idleTtlSecs !== null
                        ? `${Math.floor(idleTtlSecs / 60)} min`
                        : "10 min"}
                </strong>{" "}
                idle. Use <strong>Restart</strong> only when a runner
                is stuck.
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can restart runners.
                </div>
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {runners === null ? (
                <div className="execlaw-muted small">Loading runners…</div>
            ) : runners.length === 0 ? (
                <div className="execlaw-muted small">
                    No active runners. A new runner spawns the next time a
                    conversation receives a message.
                </div>
            ) : (
                runners.map((r) => (
                    <div
                        className="execlaw-card"
                        key={r.conversation_id}
                        data-testid="runner-row"
                        data-conversation-id={r.conversation_id}
                    >
                        <div className="d-flex align-items-center gap-2 mb-1">
                            <span className="execlaw-card__title flex-grow-1">
                                {r.principal_label ?? (
                                    <code>{r.conversation_id}</code>
                                )}
                                {r.controller_runner && (
                                    <span className="execlaw-trust-badge ms-2 is-controller">
                                        controller · always hot
                                    </span>
                                )}
                                {r.in_flight && (
                                    <span className="execlaw-trust-badge ms-2 is-known">
                                        in flight
                                    </span>
                                )}
                                {r.restart_pending && (
                                    <span className="execlaw-trust-badge ms-2 is-limited">
                                        restart pending
                                    </span>
                                )}
                                <span className="execlaw-trust-badge ms-2 is-known">
                                    {r.modality}
                                </span>
                            </span>
                            {canMutate && (
                                <Button
                                    size="sm"
                                    variant="outline-danger"
                                    disabled={busyId === r.conversation_id}
                                    onClick={() => void onRestart(r)}
                                    data-testid="runner-restart"
                                >
                                    Restart
                                </Button>
                            )}
                        </div>
                        <div className="execlaw-muted small">
                            <code>{r.conversation_id}</code>
                            {" · "}
                            {r.turn_count} turn{r.turn_count === 1 ? "" : "s"}
                            {" · last active "}
                            {fmtRelative(r.last_active_at, now)}
                            {!r.controller_runner && !r.in_flight && (
                                <>
                                    {" · idle in "}
                                    <strong>
                                        {fmtCountdown(r.idle_secs_remaining)}
                                    </strong>
                                </>
                            )}
                        </div>
                    </div>
                ))
            )}
        </div>
    );
}
