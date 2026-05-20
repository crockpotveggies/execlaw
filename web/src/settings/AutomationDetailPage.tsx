// /automations/:id detail page (M4b + M4c).
//
//   * Header: name + enabled toggle + Save/Delete.
//   * Editor: view toggle (Canvas | Code) over the same AutomationDef.
//   * Test run: collapsible drawer with sample-event picker + inline
//     trace of a non-persisted dry run.
//   * Run history: last 100 runs, per-step trace expandable inline.
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
import { AutomationCanvas } from "./AutomationCanvas";
import {
    createAutomation,
    deleteAutomation,
    emptyAutomationDef,
    getAutomation,
    getSuggestion,
    kindLabel,
    listAutomationRuns,
    listRecentBusEvents,
    testRunAutomation,
    updateAutomation,
    type AutomationDef,
    type AutomationRunView,
    type BusEventKind,
    type DryRunResult,
    type RecentBusEvent,
    type StepTrace,
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
    // M4c additions:
    const [view, setView] = useState<"canvas" | "code">("canvas");
    const [testRunOpen, setTestRunOpen] = useState<boolean>(false);
    const [recentEvents, setRecentEvents] = useState<RecentBusEvent[]>([]);
    const [selectedEventId, setSelectedEventId] = useState<string>("");
    const [testRunResult, setTestRunResult] = useState<DryRunResult | null>(null);
    const [testRunBusy, setTestRunBusy] = useState<boolean>(false);

    // Parsed definition for canvas rendering. `null` when the JSON
    // textarea contains invalid syntax — we render an inline parse
    // error instead of a broken canvas.
    const parsedDef = useMemo<{ def: AutomationDef | null; err: string | null }>(() => {
        if (defJson.trim() === "") return { def: null, err: null };
        try {
            return { def: JSON.parse(defJson) as AutomationDef, err: null };
        } catch (e) {
            return { def: null, err: (e as Error).message };
        }
    }, [defJson]);

    // Seed for create-mode — accept ?kind=<bus_event_kind> from the
    // "review suggestion" hand-off so the new automation starts on
    // the right trigger.
    const seedKind = (params.get("kind") as BusEventKind | null) ?? null;
    // M5: when the handoff carried `?suggestion=<id>`, the editor
    // fetches that suggestion and — if it has a `draft_definition`
    // (agent-drafted seed) — uses that as the JSON instead of the
    // empty graph. Falls back to `emptyAutomationDef(seedKind)` when
    // there's no draft.
    const suggestionId = params.get("suggestion");

    useEffect(() => {
        let cancelled = false;
        async function load() {
            setError(null);
            try {
                if (isNew) {
                    // Try the draft path first when the handoff
                    // carried a suggestion id. A missing draft, a
                    // resolution error, or a non-pending suggestion
                    // all fall through to the empty-seed default
                    // without surfacing a user-facing error — the
                    // page is still usable; the draft was just a
                    // nice-to-have.
                    let seedDef = emptyAutomationDef(seedKind ?? undefined);
                    let seedName = "";
                    if (suggestionId) {
                        try {
                            const s = await getSuggestion(suggestionId, token);
                            if (s.draft_definition) {
                                seedDef = s.draft_definition;
                            }
                            seedName = s.suggested_name;
                        } catch {
                            // Silent fallback to empty seed.
                        }
                    }
                    if (!cancelled) {
                        setName(seedName);
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
    }, [id, isNew, seedKind, suggestionId, token]);

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

    // Load the recent-events dropdown for the current trigger kind
    // whenever the test-run drawer is open. Cheap; runs on mount-of-
    // drawer-open + whenever the trigger kind in the JSON changes.
    useEffect(() => {
        if (!testRunOpen) return;
        const kind = parsedDef.def?.trigger?.kind;
        if (!kind) return;
        let cancelled = false;
        listRecentBusEvents(kind, 50, token)
            .then((rows) => {
                if (!cancelled) setRecentEvents(rows);
            })
            .catch((e) => {
                if (!cancelled) {
                    setError(
                        `Failed to load recent events for sample picker: ${(e as Error).message}`,
                    );
                }
            });
        return () => {
            cancelled = true;
        };
    }, [testRunOpen, parsedDef.def, token]);

    const onTestRun = useCallback(async () => {
        if (isNew) {
            setError("Save the automation before test-running.");
            return;
        }
        if (!parsedDef.def) {
            setError(
                "Definition is not valid JSON — fix and switch to code view to see the parse error.",
            );
            return;
        }
        setTestRunBusy(true);
        setError(null);
        setTestRunResult(null);
        try {
            const body = selectedEventId
                ? { event_id: selectedEventId }
                : {
                      sample_event: {
                          kind: parsedDef.def.trigger.kind,
                          source: "test-run",
                          payload: {},
                      },
                  };
            const result = await testRunAutomation(id, body, token);
            setTestRunResult(result);
        } catch (e) {
            setError((e as Error).message || "Test run failed");
        } finally {
            setTestRunBusy(false);
        }
    }, [id, isNew, parsedDef.def, selectedEventId, token]);

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

            <div className="d-flex justify-content-between align-items-end mb-2">
                <div className="small text-muted">
                    Definition — toggle Canvas / Code. JSON is the source of
                    truth; canvas is a deterministic rendering of the same
                    structure.
                </div>
                <div
                    className="btn-group btn-group-sm"
                    role="group"
                    aria-label="Editor view toggle"
                    data-testid="automation-view-toggle"
                >
                    <Button
                        variant={view === "canvas" ? "primary" : "outline-primary"}
                        size="sm"
                        onClick={() => setView("canvas")}
                        data-testid="automation-view-canvas"
                    >
                        <i className="bi bi-diagram-3 me-1" aria-hidden /> Canvas
                    </Button>
                    <Button
                        variant={view === "code" ? "primary" : "outline-primary"}
                        size="sm"
                        onClick={() => setView("code")}
                        data-testid="automation-view-code"
                    >
                        <i className="bi bi-braces me-1" aria-hidden /> Code
                    </Button>
                </div>
            </div>

            {view === "canvas" ? (
                parsedDef.def ? (
                    <AutomationCanvas
                        definition={parsedDef.def}
                        onChange={(nextDef) =>
                            setDefJson(JSON.stringify(nextDef, null, 2))
                        }
                    />
                ) : (
                    <div
                        className="border rounded p-3 small text-danger"
                        data-testid="automation-canvas-parse-error"
                    >
                        Definition is not valid JSON. Switch to Code view to
                        fix it. ({parsedDef.err})
                    </div>
                )
            ) : (
                <Form.Group className="mb-3">
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
                        Server validates on save — a 400 surfaces the
                        validator's message inline.
                    </div>
                </Form.Group>
            )}

            {!isNew && (
                <TestRunDrawer
                    isOpen={testRunOpen}
                    onToggle={() => setTestRunOpen((v) => !v)}
                    recentEvents={recentEvents}
                    selectedEventId={selectedEventId}
                    onSelectEvent={setSelectedEventId}
                    onRun={onTestRun}
                    busy={testRunBusy}
                    result={testRunResult}
                />
            )}

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

interface TestRunDrawerProps {
    isOpen: boolean;
    onToggle: () => void;
    recentEvents: RecentBusEvent[];
    selectedEventId: string;
    onSelectEvent: (id: string) => void;
    onRun: () => void;
    busy: boolean;
    result: DryRunResult | null;
}

function TestRunDrawer({
    isOpen,
    onToggle,
    recentEvents,
    selectedEventId,
    onSelectEvent,
    onRun,
    busy,
    result,
}: TestRunDrawerProps) {
    return (
        <section className="mt-4" data-testid="test-run-drawer">
            <div className="d-flex justify-content-between align-items-center">
                <h3 className="h6 mb-0">
                    <i className="bi bi-play-circle me-1" aria-hidden />
                    Test run
                </h3>
                <Button
                    variant="link"
                    size="sm"
                    onClick={onToggle}
                    data-testid="test-run-toggle"
                >
                    {isOpen ? "Hide" : "Show"}
                </Button>
            </div>
            {isOpen && (
                <div className="border rounded p-3 mt-2">
                    <Form.Group className="mb-2">
                        <Form.Label className="small text-muted mb-1">
                            Sample event
                        </Form.Label>
                        <Form.Select
                            size="sm"
                            value={selectedEventId}
                            onChange={(e) => onSelectEvent(e.target.value)}
                            data-testid="test-run-event-picker"
                        >
                            <option value="">— Synthesize empty payload —</option>
                            {recentEvents.map((ev) => (
                                <option key={ev.id} value={ev.id}>
                                    {kindLabel(ev.kind)} · {ev.source} ·{" "}
                                    {new Date(ev.received_at).toLocaleString()}
                                </option>
                            ))}
                        </Form.Select>
                        <div className="small text-muted mt-1">
                            Pick from the last 50 events of this trigger's kind.
                            "Synthesize" runs against an empty-payload event of
                            the trigger kind — useful for shape-only smoke tests.
                        </div>
                    </Form.Group>
                    <Button
                        variant="primary"
                        size="sm"
                        disabled={busy}
                        onClick={onRun}
                        data-testid="test-run-go"
                    >
                        {busy ? "Running…" : "Run"}
                    </Button>
                    {result && <TestRunResultView result={result} />}
                </div>
            )}
        </section>
    );
}

function TestRunResultView({ result }: { result: DryRunResult }) {
    const variant =
        result.outcome === "success"
            ? "success"
            : result.outcome === "failed"
              ? "danger"
              : "secondary";
    return (
        <div className="mt-3" data-testid="test-run-result">
            <div className="mb-2">
                <span
                    className={`badge bg-${variant}-subtle text-${variant}-emphasis`}
                    data-testid={`test-run-outcome-${result.outcome}`}
                >
                    {result.outcome}
                </span>
                <span className="small text-muted ms-2">
                    {result.step_traces.length} step(s)
                </span>
            </div>
            <table className="table table-sm align-middle mb-0">
                <thead>
                    <tr>
                        <th>Node</th>
                        <th>ms</th>
                        <th>Output / error</th>
                    </tr>
                </thead>
                <tbody>
                    {result.step_traces.map((t: StepTrace, i: number) => (
                        <tr key={`${t.node_id}-${i}`}>
                            <td className="font-monospace small">{t.node_id}</td>
                            <td className="small text-muted">{t.ms}</td>
                            <td>
                                {t.error ? (
                                    <span className="text-danger small">
                                        {t.error}
                                    </span>
                                ) : (
                                    <pre className="small mb-0">
                                        {JSON.stringify(t.output, null, 0)}
                                    </pre>
                                )}
                            </td>
                        </tr>
                    ))}
                </tbody>
            </table>
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
