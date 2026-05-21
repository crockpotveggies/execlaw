// Settings → Runners (Phase 16: supervisor-tracked).
//
// Each row is a per-principal-group runner container. The control
// plane manages them automatically — one container per
// `(channel, principals)` group, hot for ~10 min idle, except the
// Controller's runner which stays hot indefinitely.
//
// Operator actions:
//   * Restart → kill + respawn, workspace volume PRESERVED.
//   * Wipe    → kill + remove the workspace volume too. The group
//                row stays in the DB; only scratch files are gone.
//
// See docs/runner-design.md (and crates/server/src/runner_supervisor.rs)
// for the full lifecycle policy.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import { ApiError } from "../api/client";
import {
    listRunnerGroups,
    restartRunnerGroup,
    wipeRunnerGroup,
    type GroupRunnerView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

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

function shortGroupId(id: string): string {
    return id.length <= 12 ? id : `${id.slice(0, 8)}…${id.slice(-3)}`;
}

/// Map the supervisor's lowercase status string to a visual class
/// + a friendlier label so the operator can tell apart healthy
/// runners (`ready`) from runners that are mid-shutdown (`stopping`)
/// or already torn down but still in the registry (`dead`).
function statusBadge(status: string): { className: string; label: string } {
    switch (status) {
        case "ready":
            return { className: "is-known", label: "ready" };
        case "spawning":
            return { className: "is-known", label: "spawning" };
        case "stopping":
            return { className: "is-pending", label: "stopping…" };
        case "dead":
            return { className: "is-blocked", label: "dead" };
        default:
            return { className: "is-known", label: status };
    }
}

export function RunnersPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [runners, setRunners] = useState<GroupRunnerView[] | null>(null);
    const [idleTtlSecs, setIdleTtlSecs] = useState<number | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    const [now, setNow] = useState<number>(() =>
        Math.floor(Date.now() / 1000),
    );

    const refresh = useCallback(async () => {
        try {
            const r = await listRunnerGroups(getToken);
            setRunners(r.runners);
            setIdleTtlSecs(r.idle_ttl_secs);
            setError(null);
        } catch (e) {
            // 503 = supervisor disabled (EXECLAW_RUNNERS_ENABLED=0,
            // Docker unreachable, or runner image not built). Show a
            // friendly hint instead of a scary stack trace.
            if (e instanceof ApiError && e.status === 503) {
                setRunners([]);
                setIdleTtlSecs(null);
                setError(
                    "Runner supervisor is disabled. The chat path is " +
                        "running in legacy in-process mode. Re-enable " +
                        "via EXECLAW_RUNNERS_ENABLED=1 (and ensure the " +
                        "execlaw/runner:dev image is built) to see " +
                        "live runners on this page.",
                );
                return;
            }
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

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
        async (r: GroupRunnerView) => {
            if (
                !confirm(
                    `Restart the runner for group ${shortGroupId(
                        r.group_id,
                    )}? Any in-flight turn is dropped. The workspace volume is PRESERVED — the next message respawns onto the same scratch.`,
                )
            )
                return;
            setBusyId(r.group_id);
            try {
                await restartRunnerGroup(r.group_id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    const onWipe = useCallback(
        async (r: GroupRunnerView) => {
            if (
                !confirm(
                    `Wipe workspace for group ${shortGroupId(
                        r.group_id,
                    )}? This kills the runner AND removes the workspace volume. The conversation history (event log) is untouched.`,
                )
            )
                return;
            setBusyId(r.group_id);
            try {
                await wipeRunnerGroup(r.group_id, getToken);
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
                <div className="flex-grow-1" />
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
                The control plane manages one runner container per
                principal group automatically. The controller's runner
                stays hot indefinitely; every other runner is reaped
                after{" "}
                <strong>
                    {idleTtlSecs !== null
                        ? `${Math.floor(idleTtlSecs / 60)} min`
                        : "10 min"}
                </strong>{" "}
                idle. Use <strong>Restart</strong> to bounce a wedged
                runner (workspace preserved) or <strong>Wipe</strong>{" "}
                to also clear scratch files.
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can mutate runners.
                </div>
            )}

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            {runners === null ? (
                <div className="execlaw-muted small">Loading runners…</div>
            ) : runners.length === 0 ? (
                <div className="execlaw-muted small">
                    No active runners. A new runner spawns the next time a
                    conversation receives a message.
                </div>
            ) : (
                runners.map((r) => {
                    const idleSecs = r.controller_runner
                        ? null
                        : r.in_flight_turns > 0
                        ? null
                        : idleTtlSecs !== null
                        ? Math.max(0, idleTtlSecs - (now - r.last_active_at))
                        : null;
                    return (
                        <div
                            className="execlaw-card"
                            key={r.group_id}
                            data-testid="runner-row"
                            data-group-id={r.group_id}
                        >
                            <div className="d-flex align-items-center gap-2 mb-1">
                                <span className="execlaw-card__title flex-grow-1">
                                    <code>{shortGroupId(r.group_id)}</code>
                                    {r.controller_runner && (
                                        <span className="execlaw-trust-badge ms-2 is-controller">
                                            controller · always hot
                                        </span>
                                    )}
                                    {r.in_flight_turns > 0 && (
                                        <span className="execlaw-trust-badge ms-2 is-known">
                                            {r.in_flight_turns} in flight
                                        </span>
                                    )}
                                    {(() => {
                                        const b = statusBadge(r.status);
                                        return (
                                            <span
                                                className={`execlaw-trust-badge ms-2 ${b.className}`}
                                                data-status={r.status}
                                            >
                                                {b.label}
                                            </span>
                                        );
                                    })()}
                                </span>
                                {canMutate && (
                                    <>
                                        <Button
                                            size="sm"
                                            variant="outline-secondary"
                                            disabled={busyId === r.group_id}
                                            onClick={() => void onRestart(r)}
                                            data-testid="runner-restart"
                                        >
                                            Restart
                                        </Button>
                                        <Button
                                            size="sm"
                                            variant="outline-danger"
                                            disabled={busyId === r.group_id}
                                            onClick={() => void onWipe(r)}
                                            data-testid="runner-wipe"
                                        >
                                            Wipe
                                        </Button>
                                    </>
                                )}
                            </div>
                            <div className="execlaw-muted small">
                                started {fmtRelative(r.started_at, now)}
                                {" · last active "}
                                {fmtRelative(r.last_active_at, now)}
                                {!r.controller_runner &&
                                    r.in_flight_turns === 0 &&
                                    idleSecs !== null && (
                                        <>
                                            {" · idle in "}
                                            <strong>
                                                {fmtCountdown(idleSecs)}
                                            </strong>
                                        </>
                                    )}
                                {r.container_id && (
                                    <>
                                        {" · container "}
                                        <code>
                                            {r.container_id.slice(0, 12)}
                                        </code>
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
