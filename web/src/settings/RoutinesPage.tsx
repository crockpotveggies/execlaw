// Settings → Routines (Phase 10, MIGRATION_PLAN §5.6).
//
// Cron-shaped agent automations. The page is a list-with-editor:
//   * Top: list of routines (name, cron, status, last/next, actions).
//   * Editor: collapsible card for create / edit, with a live "next 5
//     fires" preview that hits POST /preview.
//   * Run history: per-routine drawer that lists the last N runs.
//
// The dispatch path is stubbed server-side until runner-local is real,
// so manually-fired runs land as Skipped with an explanatory error;
// the operator still sees the row appear in history immediately.

import { useCallback, useEffect, useMemo, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    createRoutine,
    deleteRoutine,
    listRoutineRuns,
    listRoutines,
    previewRoutine,
    runRoutineNow,
    updateRoutine,
    type RoutineRunView,
    type RoutineView,
    type UpsertRoutineBody,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

interface FormState {
    id: string | null; // null → create mode
    name: string;
    schedule_cron: string;
    timezone: string;
    prompt: string;
    target_conversation_id: string;
    enabled: boolean;
}

const EMPTY_FORM: FormState = {
    id: null,
    name: "",
    schedule_cron: "0 8 * * *",
    timezone: "UTC",
    prompt: "",
    target_conversation_id: "",
    enabled: true,
};

function fromView(v: RoutineView): FormState {
    return {
        id: v.id,
        name: v.name,
        schedule_cron: v.schedule_cron,
        timezone: v.timezone,
        prompt: v.prompt,
        target_conversation_id: v.target_conversation_id ?? "",
        enabled: v.enabled,
    };
}

function toUpsert(f: FormState): UpsertRoutineBody {
    return {
        name: f.name,
        schedule_cron: f.schedule_cron,
        timezone: f.timezone,
        prompt: f.prompt,
        target_conversation_id:
            f.target_conversation_id.trim() === ""
                ? null
                : f.target_conversation_id.trim(),
        enabled: f.enabled,
    };
}

function fmtTs(ts: number | null): string {
    if (ts === null) return "—";
    return new Date(ts * 1000).toLocaleString();
}

function statusBadgeClass(s: string | null): string {
    switch (s) {
        case "Success":
            return "is-known";
        case "Failed":
            return "is-blocked";
        case "Skipped":
            return "is-limited";
        case "Pending":
            return "is-pending";
        default:
            return "";
    }
}

/**
 * Format a run's duration. Prefers (finished_at - started_at) when
 * the runner populated `started_at`; falls back to
 * (finished_at - fired_at) which is queue-to-completion (still
 * meaningful — operators care about wall-clock latency, not just
 * runner CPU). Returns null for runs that haven't finished yet.
 */
function formatDuration(
    fired_at: number,
    started_at: number | null,
    finished_at: number | null,
): string | null {
    if (finished_at === null) return null;
    const startRef = started_at ?? fired_at;
    const seconds = Math.max(0, finished_at - startRef);
    if (seconds < 1) return "<1s";
    if (seconds < 60) return `${seconds}s`;
    const m = Math.floor(seconds / 60);
    const s = seconds % 60;
    if (m < 60) return s === 0 ? `${m}m` : `${m}m ${s}s`;
    const h = Math.floor(m / 60);
    const mm = m % 60;
    return mm === 0 ? `${h}h` : `${h}h ${mm}m`;
}

export function RoutinesPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [routines, setRoutines] = useState<RoutineView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [form, setForm] = useState<FormState | null>(null);
    const [busy, setBusy] = useState(false);

    const [previewFires, setPreviewFires] = useState<number[] | null>(null);
    const [previewError, setPreviewError] = useState<string | null>(null);

    const [openHistoryId, setOpenHistoryId] = useState<string | null>(null);
    const [historyRuns, setHistoryRuns] = useState<RoutineRunView[] | null>(
        null,
    );

    const refresh = useCallback(async () => {
        try {
            const r = await listRoutines(getToken);
            setRoutines(r.routines);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    // Live preview of the next 5 fires whenever cron / tz changes.
    useEffect(() => {
        if (!form) {
            setPreviewFires(null);
            return;
        }
        let cancelled = false;
        const id = window.setTimeout(async () => {
            try {
                const r = await previewRoutine(
                    form.schedule_cron,
                    form.timezone,
                    5,
                    getToken,
                );
                if (!cancelled) {
                    setPreviewFires(r.next_fires_unix);
                    setPreviewError(null);
                }
            } catch (e) {
                if (!cancelled) {
                    setPreviewFires(null);
                    setPreviewError(
                        e instanceof Error ? e.message : String(e),
                    );
                }
            }
        }, 250); // debounce — operator typing in the cron field
        return () => {
            cancelled = true;
            window.clearTimeout(id);
        };
    }, [form?.schedule_cron, form?.timezone, getToken, form]);

    const onChange = useCallback(
        <K extends keyof FormState>(field: K, value: FormState[K]) => {
            setForm((prev) => (prev ? { ...prev, [field]: value } : prev));
        },
        [],
    );

    const onCreateClick = useCallback(() => {
        setForm({ ...EMPTY_FORM });
    }, []);

    const onEditClick = useCallback((r: RoutineView) => {
        setForm(fromView(r));
    }, []);

    const onCancelEdit = useCallback(() => {
        setForm(null);
        setPreviewError(null);
    }, []);

    const onSave = useCallback(async () => {
        if (!form) return;
        setBusy(true);
        try {
            if (form.id === null) {
                await createRoutine(toUpsert(form), getToken);
            } else {
                await updateRoutine(form.id, toUpsert(form), getToken);
            }
            setForm(null);
            await refresh();
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [form, getToken, refresh]);

    const onDelete = useCallback(
        async (r: RoutineView) => {
            if (
                !confirm(
                    `Delete routine '${r.name}' and its run history? This can't be undone.`,
                )
            )
                return;
            setBusy(true);
            try {
                await deleteRoutine(r.id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [getToken, refresh],
    );

    const onRunNow = useCallback(
        async (r: RoutineView) => {
            setBusy(true);
            try {
                await runRoutineNow(r.id, getToken);
                await refresh();
                if (openHistoryId === r.id) {
                    const h = await listRoutineRuns(r.id, 50, getToken);
                    setHistoryRuns(h.runs);
                }
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [getToken, openHistoryId, refresh],
    );

    const onToggleHistory = useCallback(
        async (r: RoutineView) => {
            if (openHistoryId === r.id) {
                setOpenHistoryId(null);
                setHistoryRuns(null);
                return;
            }
            setOpenHistoryId(r.id);
            setHistoryRuns(null);
            try {
                const h = await listRoutineRuns(r.id, 50, getToken);
                setHistoryRuns(h.runs);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken, openHistoryId],
    );

    const previewLines = useMemo(() => {
        if (!previewFires) return null;
        return previewFires.map((t) => fmtTs(t));
    }, [previewFires]);

    return (
        <div data-testid="settings-routines">
            <div className="d-flex align-items-baseline gap-2 mb-2">
                <h3 className="h6 mb-0 flex-grow-1">Routines</h3>
                {form === null && (
                    <Button
                        size="sm"
                        variant="primary"
                        onClick={onCreateClick}
                        data-testid="routines-new"
                    >
                        + New routine
                    </Button>
                )}
            </div>
            <p className="execlaw-muted small mb-3">
                Cron-shaped automations the agent runs on its own. Trust
                class is the controller's; routines respect the same
                tool-allowlist as a normal turn — no quiet privilege
                escalation. Dispatch is currently stubbed (runs land as
                Skipped) until runner-local lands.
            </p>

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {form !== null && (
                <div
                    className="execlaw-card mb-3"
                    data-testid="routines-editor"
                >
                    <div className="execlaw-card__title mb-2">
                        {form.id === null
                            ? "New routine"
                            : `Editing: ${form.name}`}
                    </div>

                    <Form.Group className="mb-3" controlId="routine-name">
                        <Form.Label className="small mb-1">Name</Form.Label>
                        <Form.Control
                            type="text"
                            value={form.name}
                            onChange={(e) => onChange("name", e.target.value)}
                            placeholder="Morning summary"
                            data-testid="routine-name"
                        />
                    </Form.Group>

                    <div className="d-flex gap-2 mb-3">
                        <Form.Group
                            className="flex-grow-1"
                            controlId="routine-cron"
                        >
                            <Form.Label className="small mb-1">
                                Cron schedule (5-field)
                            </Form.Label>
                            <Form.Control
                                type="text"
                                value={form.schedule_cron}
                                onChange={(e) =>
                                    onChange("schedule_cron", e.target.value)
                                }
                                placeholder="0 8 * * *"
                                data-testid="routine-cron"
                            />
                        </Form.Group>
                        <Form.Group controlId="routine-tz">
                            <Form.Label className="small mb-1">Timezone</Form.Label>
                            <Form.Control
                                type="text"
                                value={form.timezone}
                                onChange={(e) =>
                                    onChange("timezone", e.target.value)
                                }
                                placeholder="UTC"
                                data-testid="routine-timezone"
                                style={{ minWidth: "11rem" }}
                            />
                        </Form.Group>
                    </div>

                    <div
                        className="execlaw-muted small mb-3"
                        data-testid="routine-preview"
                    >
                        {previewError ? (
                            <span className="text-danger">{previewError}</span>
                        ) : previewLines === null ? (
                            "Preview will appear here…"
                        ) : (
                            <>
                                <strong>Next 5 fires:</strong>
                                <ul className="mb-0 mt-1">
                                    {previewLines.map((s, i) => (
                                        <li key={i}>{s}</li>
                                    ))}
                                </ul>
                            </>
                        )}
                    </div>

                    <Form.Group className="mb-3" controlId="routine-prompt">
                        <Form.Label className="small mb-1">Prompt</Form.Label>
                        <Form.Control
                            as="textarea"
                            rows={4}
                            value={form.prompt}
                            onChange={(e) =>
                                onChange("prompt", e.target.value)
                            }
                            placeholder="What should the agent do when this fires?"
                            data-testid="routine-prompt"
                        />
                    </Form.Group>

                    <Form.Group className="mb-3" controlId="routine-target">
                        <Form.Label className="small mb-1">
                            Target conversation id (optional)
                        </Form.Label>
                        <Form.Control
                            type="text"
                            value={form.target_conversation_id}
                            onChange={(e) =>
                                onChange(
                                    "target_conversation_id",
                                    e.target.value,
                                )
                            }
                            placeholder="Leave blank to mint a fresh conversation each run"
                            data-testid="routine-target"
                        />
                    </Form.Group>

                    <Form.Check
                        type="switch"
                        id="routine-enabled"
                        label="Enabled"
                        checked={form.enabled}
                        onChange={(e) => onChange("enabled", e.target.checked)}
                        className="mb-3"
                        data-testid="routine-enabled"
                    />

                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => void onSave()}
                            disabled={busy}
                            data-testid="routine-save"
                        >
                            {busy
                                ? "Saving…"
                                : form.id === null
                                  ? "Create"
                                  : "Save"}
                        </Button>
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={onCancelEdit}
                            disabled={busy}
                            data-testid="routine-cancel"
                        >
                            Cancel
                        </Button>
                    </div>
                </div>
            )}

            {routines === null ? (
                <div className="execlaw-muted small">Loading routines…</div>
            ) : routines.length === 0 ? (
                <div
                    className="execlaw-muted small"
                    data-testid="routines-empty"
                >
                    No routines yet. Click "+ New routine" to define your
                    first cron-shaped automation.
                </div>
            ) : (
                routines.map((r) => (
                    <div
                        className="execlaw-card"
                        key={r.id}
                        data-testid="routine-row"
                    >
                        <div className="d-flex align-items-center gap-2 mb-2">
                            <span className="execlaw-card__title flex-grow-1">
                                {r.name}
                            </span>
                            {r.last_run_status && (
                                <span
                                    className={
                                        "execlaw-trust-badge " +
                                        statusBadgeClass(r.last_run_status)
                                    }
                                >
                                    {r.last_run_status}
                                </span>
                            )}
                            {!r.enabled && (
                                <span className="execlaw-muted small">
                                    disabled
                                </span>
                            )}
                        </div>
                        <div className="execlaw-muted small mb-2">
                            <code>{r.schedule_cron}</code> · {r.timezone}
                        </div>
                        <div className="execlaw-muted small mb-2">
                            last: {fmtTs(r.last_run_at)} · next:{" "}
                            {fmtTs(r.next_run_at)}
                        </div>
                        <div className="d-flex gap-2 flex-wrap">
                            <Button
                                size="sm"
                                variant="outline-secondary"
                                onClick={() => onEditClick(r)}
                                disabled={busy}
                                data-testid="routine-edit"
                            >
                                Edit
                            </Button>
                            <Button
                                size="sm"
                                variant="outline-primary"
                                onClick={() => void onRunNow(r)}
                                disabled={busy}
                                data-testid="routine-run-now"
                            >
                                Run now
                            </Button>
                            <Button
                                size="sm"
                                variant="outline-secondary"
                                onClick={() => void onToggleHistory(r)}
                                data-testid="routine-history"
                            >
                                {openHistoryId === r.id ? "Hide" : "History"}
                            </Button>
                            <Button
                                size="sm"
                                variant="outline-danger"
                                onClick={() => void onDelete(r)}
                                disabled={busy}
                                data-testid="routine-delete"
                            >
                                Delete
                            </Button>
                        </div>
                        {openHistoryId === r.id && (
                            <div
                                className="mt-3 execlaw-muted small"
                                data-testid="routine-history-list"
                            >
                                {historyRuns === null ? (
                                    "Loading run history…"
                                ) : historyRuns.length === 0 ? (
                                    "No runs yet."
                                ) : (
                                    <ul className="list-unstyled mb-0">
                                        {historyRuns.map((run) => (
                                            <li
                                                key={run.id}
                                                className="mb-1"
                                                data-testid="routine-history-row"
                                            >
                                                <span
                                                    className={
                                                        "execlaw-trust-badge me-2 " +
                                                        statusBadgeClass(
                                                            run.status,
                                                        )
                                                    }
                                                >
                                                    {run.status}
                                                </span>
                                                fired {fmtTs(run.fired_at)}
                                                {(() => {
                                                    const dur = formatDuration(
                                                        run.fired_at,
                                                        run.started_at,
                                                        run.finished_at,
                                                    );
                                                    return dur ? (
                                                        <span
                                                            className="ms-2"
                                                            data-testid="routine-history-duration"
                                                        >
                                                            · {dur}
                                                        </span>
                                                    ) : null;
                                                })()}
                                                {run.error && (
                                                    <span className="text-danger ms-2">
                                                        {run.error}
                                                    </span>
                                                )}
                                            </li>
                                        ))}
                                    </ul>
                                )}
                            </div>
                        )}
                    </div>
                ))
            )}
        </div>
    );
}
