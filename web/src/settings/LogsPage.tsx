// Logs page — reads the daily-rotated `execlaw.jsonl.<DATE>` files the
// tracing file appender writes to disk, via `/api/admin/logs`. Filters
// (level floor, plugin id, conversation id, time range) and a manual
// Refresh button on top; optional 10s auto-refresh for follow mode.

import { useCallback, useEffect, useRef, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import { getLogs, type LogEntry } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

const LEVELS = ["", "trace", "debug", "info", "warn", "error"] as const;
type Level = (typeof LEVELS)[number];

export function LogsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [level, setLevel] = useState<Level>("");
    const [pluginId, setPluginId] = useState("");
    const [conversationId, setConversationId] = useState("");
    // datetime-local input values (local time, no TZ). Empty = unbounded.
    const [sinceLocal, setSinceLocal] = useState("");
    const [untilLocal, setUntilLocal] = useState("");
    const [entries, setEntries] = useState<LogEntry[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [follow, setFollow] = useState(false);
    const [loading, setLoading] = useState(false);

    const fetchOnce = useCallback(async () => {
        setLoading(true);
        try {
            const r = await getLogs(
                {
                    level: level || undefined,
                    plugin_id: pluginId.trim() || undefined,
                    conversation_id: conversationId.trim() || undefined,
                    since_ms: localToMs(sinceLocal),
                    until_ms: localToMs(untilLocal),
                    limit: 500,
                },
                getToken,
            );
            setEntries(r.entries);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setLoading(false);
        }
    }, [conversationId, getToken, level, pluginId, sinceLocal, untilLocal]);

    useEffect(() => {
        void fetchOnce();
    }, [fetchOnce]);

    const tickRef = useRef<ReturnType<typeof setInterval> | null>(null);
    useEffect(() => {
        if (!follow) {
            if (tickRef.current) clearInterval(tickRef.current);
            tickRef.current = null;
            return;
        }
        tickRef.current = setInterval(() => {
            void fetchOnce();
        }, 10_000);
        return () => {
            if (tickRef.current) clearInterval(tickRef.current);
            tickRef.current = null;
        };
    }, [follow, fetchOnce]);

    return (
        <div data-testid="settings-logs">
            <h3 className="h6 mb-3">Logs</h3>

            <div className="execlaw-card">
                <Form className="row g-2 align-items-end">
                    <Form.Group className="col-sm-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Min level
                        </Form.Label>
                        <Form.Select
                            value={level}
                            onChange={(e) => setLevel(e.target.value as Level)}
                            data-testid="logs-level"
                        >
                            <option value="">all</option>
                            {LEVELS.filter((l) => l).map((l) => (
                                <option key={l} value={l}>
                                    {l}
                                </option>
                            ))}
                        </Form.Select>
                    </Form.Group>
                    <Form.Group className="col-sm-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Plugin id
                        </Form.Label>
                        <Form.Control
                            value={pluginId}
                            onChange={(e) => setPluginId(e.target.value)}
                            placeholder="any"
                        />
                    </Form.Group>
                    <Form.Group className="col-sm-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Conversation id
                        </Form.Label>
                        <Form.Control
                            value={conversationId}
                            onChange={(e) => setConversationId(e.target.value)}
                            placeholder="any"
                        />
                    </Form.Group>
                    <Form.Group className="col-sm-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            From
                        </Form.Label>
                        <Form.Control
                            type="datetime-local"
                            value={sinceLocal}
                            onChange={(e) => setSinceLocal(e.target.value)}
                            data-testid="logs-since"
                        />
                    </Form.Group>
                    <Form.Group className="col-sm-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            To
                        </Form.Label>
                        <Form.Control
                            type="datetime-local"
                            value={untilLocal}
                            onChange={(e) => setUntilLocal(e.target.value)}
                            data-testid="logs-until"
                        />
                    </Form.Group>
                    <div className="col-sm-1 d-flex align-items-end">
                        <Button
                            size="sm"
                            variant="outline-secondary"
                            onClick={() => void fetchOnce()}
                            disabled={loading}
                            data-testid="logs-refresh"
                        >
                            {loading ? "…" : "Refresh"}
                        </Button>
                    </div>
                    <Form.Group className="col-12 d-flex gap-3 mt-2">
                        <Form.Check
                            type="switch"
                            id="logs-follow"
                            label="Auto-refresh (10s)"
                            checked={follow}
                            onChange={(e) => setFollow(e.target.checked)}
                        />
                        <button
                            type="button"
                            className="btn btn-link btn-sm p-0 execlaw-muted"
                            onClick={() => {
                                setLevel("");
                                setPluginId("");
                                setConversationId("");
                                setSinceLocal("");
                                setUntilLocal("");
                            }}
                        >
                            Clear filters
                        </button>
                    </Form.Group>
                </Form>
            </div>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            <div className="execlaw-card">
                {entries === null ? (
                    <div className="execlaw-muted small">Loading…</div>
                ) : entries.length === 0 ? (
                    <div className="execlaw-muted small">
                        No log entries match the current filter.
                    </div>
                ) : (
                    entries.map((e, i) => (
                        <div className="execlaw-log-row" key={i}>
                            <span className="execlaw-log-row__ts">
                                {formatTs(e.ts_ms)}
                            </span>
                            <span
                                className={`execlaw-log-row__level is-${e.level.toLowerCase()}`}
                            >
                                {e.level}
                            </span>
                            <span
                                className="execlaw-log-row__target"
                                title={e.target}
                            >
                                {e.target}
                            </span>
                            <span className="execlaw-log-row__msg">{e.message}</span>
                        </div>
                    ))
                )}
            </div>
        </div>
    );
}

function formatTs(ms: number): string {
    try {
        const d = new Date(ms);
        return `${d.toLocaleDateString()} ${d.toLocaleTimeString()}`;
    } catch {
        return String(ms);
    }
}

// `<input type="datetime-local">` returns "YYYY-MM-DDTHH:MM" in local
// time. `new Date(s)` parses that as local time, which gives the
// correct epoch ms for filtering UTC-stamped log entries.
function localToMs(s: string): number | undefined {
    if (!s) return undefined;
    const ms = new Date(s).getTime();
    return Number.isFinite(ms) ? ms : undefined;
}
