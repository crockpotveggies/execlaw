// /automations/:id detail page (M4b).
//
// Two panes:
//
//   * Top: name + enabled toggle + definition editor (JSON textarea in
//     this milestone; React Flow canvas in a follow-up). The editor
//     parses on save and forwards the server's validator message
//     verbatim on a 400.
//
//   * Bottom: run history drawer (last 100 runs, per-step trace
//     expandable inline). Read-only.
//
// The `id` prop carries either an existing automation id OR the
// literal "new" (used by the suggestion-action handoff). When `id`
// is "new" we seed an empty definition + show no run history.

import {
    useCallback,
    useEffect,
    useMemo,
    useState,
    type ChangeEvent,
} from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import {
    createAutomation,
    deleteAutomation,
    emptyAutomationDef,
    getAutomation,
    listAutomationRuns,
    updateAutomation,
    type AutomationDef,
    type AutomationRunView,
    type BusEventKind,
} from "../api/automations";

interface Props {
    /** Automation id or the literal "new" for create mode. */
    id: string;
}

export function AutomationDetailPage({ id }: Props) {
    const auth = useAuth();
    const token = auth.getAccessToken;
    const navigate = useNavigate();
    const [params] = useSearchParams();
    const isNew = id === "new";

    const [name, setName] = useState<string>("");
    const [enabled, setEnabled] = useState<boolean>(true);
    const [defJson, setDefJson] = useState<string>("");
    const [runs, setRuns] = useState<AutomationRunView[]>([]);
    const [loaded, setLoaded] = useState<boolean>(false);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState<boolean>(false);
    const [expandedRunId, setExpandedRunId] = useState<string | null>(null);

    // Seed for create-mode — accept ?kind=<bus_event_kind> from the
    // "review suggestion" hand-off so the new automation starts on
    // the right trigger.
    const seedKind = (params.get("kind") as BusEventKind | null) ?? null;

    useEffect(() => {
        let cancelled = false;
        async function load() {
            setError(null);
            try {
                if (isNew) {
                    const seedDef = emptyAutomationDef(seedKind ?? undefined);
                    if (!cancelled) {
                        setName("");
                        setEnabled(true);
                        setDefJson(JSON.stringify(seedDef, null, 2));
                        setRuns([]);
                        setLoaded(true);
                    }
                    return;
                }
                const [row, runRows] = await Promise.all([
                    getAutomation(id, token),
                    listAutomationRuns(id, token),
                ]);
                if (cancelled) return;
                setName(row.name);
                setEnabled(row.enabled);
                setDefJson(JSON.stringify(row.definition, null, 2));
                setRuns(runRows);
                setLoaded(true);
            } catch (e) {
                if (!cancelled) {
                    setError((e as Error).message || "Failed to load automation");
                    setLoaded(true);
                }
            }
        }
        void load();
        return () => {
            cancelled = true;
        };
    }, [id, isNew, seedKind, token]);

    const onSave = useCallback(async () => {
        setError(null);
        setBusy(true);
        let parsed: AutomationDef;
        try {
            parsed = JSON.parse(defJson) as AutomationDef;
        } catch (e) {
            setError(`Definition is not valid JSON: ${(e as Error).message}`);
            setBusy(false);
            return;
        }
        const body = { name: name.trim(), enabled, definition: parsed };
        try {
            if (isNew) {
                const created = await createAutomation(body, token);
                navigate(`/automations/${created.id}`, { replace: true });
            } else {
                await updateAutomation(id, body, token);
                // Refresh runs in case the save itself fired any (it
                // doesn't today, but keeps the page honest).
                const refreshed = await listAutomationRuns(id, token);
                setRuns(refreshed);
            }
        } catch (e) {
            setError((e as Error).message || "Save failed");
        } finally {
            setBusy(false);
        }
    }, [defJson, name, enabled, isNew, id, token, navigate]);

    const onDelete = useCallback(async () => {
        if (isNew) return;
        if (!confirm("Delete this automation? This cannot be undone.")) return;
        setBusy(true);
        try {
            await deleteAutomation(id, token);
            navigate("/automations", { replace: true });
        } catch (e) {
            setError((e as Error).message || "Delete failed");
            setBusy(false);
        }
    }, [id, isNew, navigate, token]);

    if (!loaded) {
        return (
            <div className="execlaw-muted small p-3" data-testid="detail-loading">
                Loading…
            </div>
        );
    }

    return (
        <div className="execlaw-automation-detail" data-testid="automation-detail">
            <ErrorBanner message={error} onDismiss={() => setError(null)} />

            <div className="d-flex justify-content-between align-items-start mb-3">
                <div className="flex-grow-1 me-3">
                    <Form.Group className="mb-2">
                        <Form.Label className="small text-muted mb-1">
                            Name
                        </Form.Label>
                        <Form.Control
                            type="text"
                            value={name}
                            onChange={(e: ChangeEvent<HTMLInputElement>) =>
                                setName(e.target.value)
                            }
                            placeholder="e.g. Ring driveway watch"
                            data-testid="automation-name-input"
                        />
                    </Form.Group>
                    <Form.Check
                        type="switch"
                        id="enabled-switch"
                        label="Enabled"
                        checked={enabled}
                        onChange={(e: ChangeEvent<HTMLInputElement>) =>
                            setEnabled(e.target.checked)
                        }
                        data-testid="automation-enabled-switch"
                    />
                </div>
                <div className="d-flex gap-2">
                    <Button
                        variant="primary"
                        size="sm"
                        disabled={busy || name.trim() === ""}
                        onClick={onSave}
                        data-testid="automation-save-btn"
                    >
                        Save
                    </Button>
                    {!isNew && (
                        <Button
                            variant="outline-danger"
                            size="sm"
                            disabled={busy}
                            onClick={onDelete}
                            data-testid="automation-delete-btn"
                        >
                            Delete
                        </Button>
                    )}
                </div>
            </div>

            <Form.Group className="mb-3">
                <Form.Label className="small text-muted mb-1">
                    Definition (JSON)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    value={defJson}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        setDefJson(e.target.value)
                    }
                    rows={18}
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="automation-def-textarea"
                />
                <div className="small text-muted mt-1">
                    Trigger + typed nodes + edges. Server validates on save —
                    a 400 surfaces the validator's message inline. The visual
                    canvas editor lands in a follow-up; the JSON shape is the
                    source of truth.
                </div>
            </Form.Group>

            {!isNew && (
                <RunsDrawer
                    runs={runs}
                    expandedRunId={expandedRunId}
                    onToggle={(rid) =>
                        setExpandedRunId((curr) => (curr === rid ? null : rid))
                    }
                />
            )}
        </div>
    );
}

interface RunsDrawerProps {
    runs: AutomationRunView[];
    expandedRunId: string | null;
    onToggle: (id: string) => void;
}

function RunsDrawer({ runs, expandedRunId, onToggle }: RunsDrawerProps) {
    const sortedRuns = useMemo(
        () => [...runs].sort((a, b) => b.started_at - a.started_at),
        [runs],
    );

    if (sortedRuns.length === 0) {
        return (
            <section className="mt-4" data-testid="runs-drawer">
                <h3 className="h6">Recent runs</h3>
                <div className="execlaw-muted small">No runs yet.</div>
            </section>
        );
    }

    return (
        <section className="mt-4" data-testid="runs-drawer">
            <h3 className="h6">Recent runs ({sortedRuns.length})</h3>
            <table className="table table-sm align-middle">
                <thead>
                    <tr>
                        <th>Started</th>
                        <th>Status</th>
                        <th>Steps</th>
                        <th aria-label="expand" />
                    </tr>
                </thead>
                <tbody>
                    {sortedRuns.map((r) => (
                        <RunRow
                            key={r.id}
                            run={r}
                            expanded={expandedRunId === r.id}
                            onToggle={() => onToggle(r.id)}
                        />
                    ))}
                </tbody>
            </table>
        </section>
    );
}

function RunRow({
    run,
    expanded,
    onToggle,
}: {
    run: AutomationRunView;
    expanded: boolean;
    onToggle: () => void;
}) {
    return (
        <>
            <tr data-testid={`run-row-${run.id}`}>
                <td className="text-muted small">
                    {new Date(run.started_at).toLocaleString()}
                </td>
                <td>
                    <StatusBadge status={run.status} />
                </td>
                <td className="text-muted small">{run.step_traces.length}</td>
                <td className="text-end">
                    <Button
                        variant="link"
                        size="sm"
                        onClick={onToggle}
                        data-testid={`run-${run.id}-toggle`}
                    >
                        {expanded ? "Hide" : "Trace"}
                    </Button>
                </td>
            </tr>
            {expanded && (
                <tr data-testid={`run-${run.id}-trace`}>
                    <td colSpan={4}>
                        <pre className="bg-light p-2 small mb-0 border rounded">
                            {JSON.stringify(run.step_traces, null, 2)}
                        </pre>
                    </td>
                </tr>
            )}
        </>
    );
}

function StatusBadge({ status }: { status: AutomationRunView["status"] }) {
    const variant =
        status === "success"
            ? "success"
            : status === "failed"
              ? "danger"
              : status === "skipped"
                ? "secondary"
                : "info";
    return (
        <span
            className={`badge bg-${variant}-subtle text-${variant}-emphasis`}
            data-testid={`run-status-${status}`}
        >
            {status}
        </span>
    );
}
