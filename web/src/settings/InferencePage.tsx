// Settings → Inference observability page (M5).
//
// Per-consumer slice of LLM call load. Six columns:
//
//   * Consumer (chat / routines / research / automations / other)
//   * In flight (current outstanding calls)
//   * Total calls (lifetime of the server process)
//   * Failures (subset of total_calls that returned Err)
//   * p50, p95 (over the last 256 calls per consumer)
//
// Polls every 5s by default; the operator can pause via the toggle
// (handy when staring at the page to debug a regression — no flashing
// numbers).

import { useCallback, useEffect, useRef, useState } from "react";
import Button from "react-bootstrap/Button";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import {
    consumerLabel,
    getInferenceMetrics,
    type MetricsSnapshot,
} from "../api/inference";

const REFRESH_MS = 5_000;

export function InferencePage() {
    const auth = useAuth();
    const token = auth.getAccessToken;
    const [snap, setSnap] = useState<MetricsSnapshot | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [autoRefresh, setAutoRefresh] = useState<boolean>(true);
    const timerRef = useRef<number | null>(null);

    const fetchNow = useCallback(async () => {
        setError(null);
        try {
            const s = await getInferenceMetrics(token);
            setSnap(s);
        } catch (e) {
            setError((e as Error).message || "Failed to load inference metrics");
        }
    }, [token]);

    useEffect(() => {
        void fetchNow();
    }, [fetchNow]);

    useEffect(() => {
        if (!autoRefresh) {
            if (timerRef.current !== null) {
                window.clearInterval(timerRef.current);
                timerRef.current = null;
            }
            return;
        }
        timerRef.current = window.setInterval(() => {
            void fetchNow();
        }, REFRESH_MS);
        return () => {
            if (timerRef.current !== null) {
                window.clearInterval(timerRef.current);
                timerRef.current = null;
            }
        };
    }, [autoRefresh, fetchNow]);

    return (
        <div data-testid="inference-page">
            <ErrorBanner message={error} onDismiss={() => setError(null)} />
            <div className="d-flex justify-content-between align-items-center mb-3">
                <p className="text-muted small mb-0">
                    Per-consumer LLM call load. The same inference backend
                    serves chat, routines, research, and automations — this
                    table tells you who's driving the load.
                </p>
                <div className="d-flex gap-2 align-items-center">
                    <div className="form-check form-switch mb-0">
                        <input
                            className="form-check-input"
                            type="checkbox"
                            role="switch"
                            id="inference-autorefresh"
                            checked={autoRefresh}
                            onChange={(e) => setAutoRefresh(e.target.checked)}
                            data-testid="inference-autorefresh-switch"
                        />
                        <label
                            className="form-check-label small"
                            htmlFor="inference-autorefresh"
                        >
                            Auto-refresh ({Math.round(REFRESH_MS / 1000)}s)
                        </label>
                    </div>
                    <Button
                        variant="outline-secondary"
                        size="sm"
                        onClick={() => void fetchNow()}
                        data-testid="inference-refresh-btn"
                    >
                        <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                        Refresh
                    </Button>
                </div>
            </div>

            {snap === null ? (
                <div className="execlaw-muted small p-3" data-testid="inference-loading">
                    Loading…
                </div>
            ) : snap.consumers.length === 0 ? (
                <div
                    className="execlaw-muted small p-3 border rounded"
                    data-testid="inference-empty"
                >
                    No LLM calls observed yet. Counters populate as chat
                    turns, automation runs, routines, or research jobs make
                    inference requests.
                </div>
            ) : (
                <ConsumersTable snap={snap} />
            )}
        </div>
    );
}

function ConsumersTable({ snap }: { snap: MetricsSnapshot }) {
    return (
        <table
            className="table table-sm align-middle"
            data-testid="inference-consumers-table"
        >
            <thead>
                <tr>
                    <th>Consumer</th>
                    <th className="text-end">In flight</th>
                    <th className="text-end">Total calls</th>
                    <th className="text-end">Failures</th>
                    <th className="text-end">p50</th>
                    <th className="text-end">p95</th>
                </tr>
            </thead>
            <tbody>
                {snap.consumers.map((c) => (
                    <tr
                        key={c.consumer}
                        data-testid={`inference-row-${c.consumer}`}
                    >
                        <td>{consumerLabel(c.consumer)}</td>
                        <td className="text-end font-monospace">
                            {c.in_flight}
                        </td>
                        <td className="text-end font-monospace">
                            {c.total_calls}
                        </td>
                        <td className="text-end font-monospace">
                            {c.total_failures}
                            {c.total_calls > 0 && c.total_failures > 0 && (
                                <span className="small text-muted ms-1">
                                    (
                                    {(
                                        (c.total_failures / c.total_calls) *
                                        100
                                    ).toFixed(1)}
                                    %)
                                </span>
                            )}
                        </td>
                        <td className="text-end font-monospace">
                            {fmtMs(c.p50_ms)}
                        </td>
                        <td className="text-end font-monospace">
                            {fmtMs(c.p95_ms)}
                        </td>
                    </tr>
                ))}
            </tbody>
        </table>
    );
}

function fmtMs(ms: number | null): string {
    if (ms === null) return "—";
    if (ms >= 1000) return `${(ms / 1000).toFixed(2)}s`;
    return `${ms}ms`;
}
