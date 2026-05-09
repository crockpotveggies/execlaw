// Settings → Alerts (Phase 9.1, MIGRATION_PLAN §10).
//
// Operator-facing alert list with ack + resolve actions. Storage and
// dedup live in `crates/core::alerts`; this page just reads + drives
// state transitions through `/api/admin/alerts`.
//
// The SPA defaults to Firing-only because that's the actionable
// subset; "Show resolved/acked" toggles widen the filter for audit.

import { useCallback, useEffect, useMemo, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    ackAlert,
    listAlerts,
    resolveAlert,
    type AlertSeverity,
    type AlertStatus,
    type AlertView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

const SEVERITY_BADGE: Record<AlertSeverity, string> = {
    Critical: "is-blocked",
    Error: "is-pending",
    Warning: "is-limited",
    Info: "is-known",
};

function severityRank(s: AlertSeverity): number {
    switch (s) {
        case "Critical":
            return 0;
        case "Error":
            return 1;
        case "Warning":
            return 2;
        case "Info":
            return 3;
    }
}

export function AlertsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [alerts, setAlerts] = useState<AlertView[] | null>(null);
    const [firingCount, setFiringCount] = useState<number | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [includeClosed, setIncludeClosed] = useState(false);
    const [busyId, setBusyId] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listAlerts(
                {
                    status: includeClosed
                        ? undefined
                        : (["Firing", "Snoozed"] as AlertStatus[]),
                    limit: 200,
                },
                getToken,
            );
            // Sort by severity (Critical first) then by last_seen desc
            // — noisy lower-severity rows sink below the actionable
            // top.
            const sorted = [...r.alerts].sort((a, b) => {
                const sevDiff = severityRank(a.severity) - severityRank(b.severity);
                if (sevDiff !== 0) return sevDiff;
                return b.last_seen_at - a.last_seen_at;
            });
            setAlerts(sorted);
            setFiringCount(r.firing_count);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken, includeClosed]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onAck = useCallback(
        async (id: string) => {
            setBusyId(id);
            try {
                await ackAlert(id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    const onResolve = useCallback(
        async (id: string) => {
            setBusyId(id);
            try {
                await resolveAlert(id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    const headerCount = useMemo(() => {
        if (firingCount === null) return null;
        return firingCount === 1 ? "1 firing" : `${firingCount} firing`;
    }, [firingCount]);

    return (
        <div data-testid="settings-alerts">
            <div className="d-flex align-items-baseline gap-2 mb-2">
                <h3 className="h6 mb-0 flex-grow-1">Alerts</h3>
                {headerCount && (
                    <span className="execlaw-muted small">{headerCount}</span>
                )}
            </div>
            <p className="execlaw-muted small mb-3">
                Operational anomalies — plugin failures, OAuth expiries,
                rate-limits, runner crashes. Ack to silence the badge;
                resolve when the underlying cause is fixed.
            </p>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            <Form.Check
                type="switch"
                id="alerts-include-closed"
                label="Include resolved / acked"
                checked={includeClosed}
                onChange={(e) => setIncludeClosed(e.target.checked)}
                className="mb-3"
                data-testid="alerts-include-closed"
            />

            {alerts === null ? (
                <div className="execlaw-muted small">Loading alerts…</div>
            ) : alerts.length === 0 ? (
                <div
                    className="execlaw-muted small"
                    data-testid="alerts-empty"
                >
                    {includeClosed
                        ? "No alerts on file. Anomalies will land here as plugins and core hooks fire."
                        : "Nothing firing right now."}
                </div>
            ) : (
                alerts.map((a) => (
                    <div
                        className="execlaw-card"
                        key={a.id}
                        data-testid="alert-row"
                    >
                        <div className="d-flex align-items-center gap-2 mb-2">
                            <span
                                className={`execlaw-trust-badge ${SEVERITY_BADGE[a.severity]}`}
                                data-testid="alert-severity"
                            >
                                {a.severity}
                            </span>
                            <span className="execlaw-card__title flex-grow-1">
                                {a.title}
                            </span>
                            <span
                                className="execlaw-muted small"
                                data-testid="alert-status"
                            >
                                {a.status}
                                {a.occurrence_count > 1 && (
                                    <> · ×{a.occurrence_count}</>
                                )}
                            </span>
                        </div>
                        {a.detail && (
                            <p className="small mb-2">{a.detail}</p>
                        )}
                        <div className="execlaw-muted small mb-2">
                            <code>{a.source}</code> · last seen{" "}
                            {new Date(a.last_seen_at * 1000).toLocaleString()}
                        </div>
                        {a.status === "Firing" && (
                            <div className="d-flex gap-2">
                                <Button
                                    size="sm"
                                    variant="outline-secondary"
                                    disabled={busyId === a.id}
                                    onClick={() => void onAck(a.id)}
                                    data-testid="alert-ack"
                                >
                                    <i className="bi bi-check2 me-2" aria-hidden />
                                    Ack
                                </Button>
                                <Button
                                    size="sm"
                                    variant="outline-success"
                                    disabled={busyId === a.id}
                                    onClick={() => void onResolve(a.id)}
                                    data-testid="alert-resolve"
                                >
                                    <i className="bi bi-check2-all me-2" aria-hidden />
                                    Resolve
                                </Button>
                            </div>
                        )}
                        {a.status === "Acked" && (
                            <Button
                                size="sm"
                                variant="outline-success"
                                disabled={busyId === a.id}
                                onClick={() => void onResolve(a.id)}
                                data-testid="alert-resolve"
                            >
                                <i className="bi bi-check2-all me-2" aria-hidden />
                                Resolve
                            </Button>
                        )}
                    </div>
                ))
            )}
        </div>
    );
}
