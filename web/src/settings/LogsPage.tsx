// Logs page — paginated /api/admin/logs feed with level filters.
// Auto-refreshes when "follow" is on (10s tick). The real-time feed
// over WS lands later; for the Phase-6 SPA this is sufficient.

import { useCallback, useEffect, useRef, useState } from "react";
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
    const [entries, setEntries] = useState<LogEntry[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [follow, setFollow] = useState(false);

    const fetchOnce = useCallback(async () => {
        try {
            const r = await getLogs(
                {
                    level: level || undefined,
                    plugin_id: pluginId.trim() || undefined,
                    conversation_id: conversationId.trim() || undefined,
                    limit: 200,
                },
                getToken,
            );
            setEntries(r.entries);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [conversationId, getToken, level, pluginId]);

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
                    <Form.Group className="col-sm-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Level
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
                    <Form.Group className="col-sm-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Plugin id
                        </Form.Label>
                        <Form.Control
                            value={pluginId}
                            onChange={(e) => setPluginId(e.target.value)}
                            placeholder="any"
                        />
                    </Form.Group>
                    <Form.Group className="col-sm-4">
                        <Form.Label className="execlaw-muted small mb-1">
                            Conversation id
                        </Form.Label>
                        <Form.Control
                            value={conversationId}
                            onChange={(e) => setConversationId(e.target.value)}
                            placeholder="any"
                        />
                    </Form.Group>
                    <Form.Group className="col-sm-2 d-flex align-items-center mt-3">
                        <Form.Check
                            type="switch"
                            id="logs-follow"
                            label="Auto-refresh"
                            checked={follow}
                            onChange={(e) => setFollow(e.target.checked)}
                        />
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
        return d.toLocaleTimeString();
    } catch {
        return String(ms);
    }
}
