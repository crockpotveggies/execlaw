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
    useRef,
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
    type EventEnvelope,
    type RecentBusEvent,
    type StepTrace,
    type TrustClass,
} from "../api/automations";

/**
 * Live trace event surfaced from the FlowChannelHub SSE feed. Matches
 * the Rust `FlowChannelEvent` union — but tagged narrowly here so the
 * render code can switch on `t` without needing the full Rust schema
 * mirror. We keep the unparsed JSON in `raw` for debugging.
 */
interface LiveTraceEvent {
    /** Local ms timestamp the SPA received the frame. */
    ts: number;
    /** SSE `event:` header (e.g. "node_started", "agent_text_delta"). */
    t: string;
    /** Decoded data payload. */
    data: Record<string, unknown>;
}

/**
 * Compose an `EventEnvelope` from the test-run drawer's three
 * dropdowns. `system_internal` (the default) returns the same shape
 * as the server's `EventEnvelope::system_internal()` so this stays a
 * no-op when the operator hasn't customized the envelope. */
function buildSampleEnvelope(
    originKind: string,
    identityKind: string,
    identityTrust: string,
): EventEnvelope {
    // Origin
    let origin: EventEnvelope["origin"];
    switch (originKind) {
        case "web_socket_session":
            origin = { kind: "web_socket_session", session_id: "test-session" };
            break;
        case "chat_append":
            origin = { kind: "chat_append", thread_id: "test-thread" };
            break;
        case "alert":
            origin = { kind: "alert" };
            break;
        case "none":
            origin = { kind: "none" };
            break;
        default:
            // Default = "system" / no replyable origin. We map this to
            // OriginRef::None which is what `system_internal()` uses.
            origin = { kind: "none" };
            break;
    }
    // Identity
    let identity: EventEnvelope["identity"];
    const trust = (identityTrust as TrustClass);
    switch (identityKind) {
        case "principal":
            identity = { kind: "principal", id: "test-operator", trust };
            break;
        case "external":
            identity = {
                kind: "external",
                plugin_id: "test-plugin",
                handle: "test-handle",
                trust,
            };
            break;
        case "system":
        default:
            identity = { kind: "system" };
            break;
    }
    return {
        origin,
        identity,
        correlation_id: `test-${crypto.randomUUID()}`,
        parent_event_id: null,
    };
}

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
    // M6 — provenance fields, surfaced for the delete-button guard
    // and (future) the diff-on-upgrade card.
    const [source, setSource] = useState<string>("operator");
    const [isDefault, setIsDefault] = useState<boolean>(false);
    // M4c additions:
    const [view, setView] = useState<"canvas" | "code">("canvas");
    const [testRunOpen, setTestRunOpen] = useState<boolean>(false);
    const [recentEvents, setRecentEvents] = useState<RecentBusEvent[]>([]);
    const [selectedEventId, setSelectedEventId] = useState<string>("");
    const [testRunResult, setTestRunResult] = useState<DryRunResult | null>(null);
    const [testRunBusy, setTestRunBusy] = useState<boolean>(false);
    // Audit fix #8: live SSE trace events buffered as the run executes.
    // Cleared at the start of each new run. The Eventsource ref is
    // held so the unmount cleanup can close it even if the run is
    // still streaming.
    const [liveTrace, setLiveTrace] = useState<LiveTraceEvent[]>([]);
    const liveSourceRef = useRef<EventSource | null>(null);
    useEffect(() => {
        // Close any open SSE on page unmount.
        return () => {
            liveSourceRef.current?.close();
            liveSourceRef.current = null;
        };
    }, []);
    // Audit fix #9: envelope override used to test trigger filters that
    // gate on `event.envelope.origin.kind`, `identity.trust`, etc. The
    // form composes these strings into an `EventEnvelope` JSON object
    // when the operator hits Run. `null` means "let the server default
    // to system_internal()".
    const [sampleOriginKind, setSampleOriginKind] = useState<string>("system_internal");
    const [sampleIdentityKind, setSampleIdentityKind] = useState<string>("system");
    const [sampleIdentityTrust, setSampleIdentityTrust] = useState<string>("controller");
    const [samplePayloadJson, setSamplePayloadJson] = useState<string>("{}");
    const [samplePayloadErr, setSamplePayloadErr] = useState<string | null>(null);

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
                setSource(row.source ?? "operator");
                setIsDefault(Boolean(row.is_default));
                setLoaded(true);
            } catch (e) {
                if (!cancelled) {
                    setError((e as Error).message || "Failed to load flow");
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
            setError("Save the flow before test-running.");
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
        setLiveTrace([]);
        // Mint the run id client-side and open SSE FIRST so we don't
        // race the executor publishing NodeStarted events before our
        // subscriber attaches to the broadcast channel. The endpoint
        // accepts un-keyed `flow-runs/{id}/events`; the hub creates
        // the broadcast channel on first subscribe.
        const clientRunId = `dry-${crypto.randomUUID()}`;
        // Close any previous EventSource before opening a new one.
        liveSourceRef.current?.close();
        // EventSource is missing under jsdom (test env) and could be
        // missing in older WebViews — gate the live-trace plumbing
        // behind a feature detect. The POST response still carries
        // the full DryRunResult so the run remains usable; we just
        // don't get the per-step streaming UI.
        if (typeof EventSource !== "undefined") {
            const es = new EventSource(
                `/api/automations/flow-runs/${encodeURIComponent(clientRunId)}/events`,
            );
            liveSourceRef.current = es;
            const eventTypes = [
                "node_started",
                "node_finished",
                "agent_turn_started",
                "agent_text_delta",
                "agent_tool_call_delta",
                "agent_turn_finished",
                "reply_routed",
                "run_finished",
            ];
            for (const t of eventTypes) {
                es.addEventListener(t, (ev) => {
                    let data: Record<string, unknown> = {};
                    try {
                        data = JSON.parse((ev as MessageEvent).data);
                    } catch {
                        // Leave data empty — the SSE channel guarantees
                        // valid JSON per the server but defensive parse
                        // keeps a single bad frame from crashing the UI.
                    }
                    setLiveTrace((prev) => [...prev, { ts: Date.now(), t, data }]);
                    if (t === "run_finished") {
                        es.close();
                        liveSourceRef.current = null;
                    }
                });
            }
            es.onerror = () => {
                // Broadcast channels may close cleanly when the run
                // finishes BEFORE the run_finished event lands (race in
                // the publish/close path). We don't surface this — the
                // POST response carries the canonical outcome.
                es.close();
                if (liveSourceRef.current === es) {
                    liveSourceRef.current = null;
                }
            };
        }
        try {
            let body: Record<string, unknown>;
            if (selectedEventId) {
                body = { event_id: selectedEventId, client_run_id: clientRunId };
            } else {
                // Synthesize-mode: build payload + envelope from the
                // form. Bail with an inline error if the payload JSON
                // is bad so the run never hits the server with junk.
                let payload: unknown = {};
                try {
                    payload = samplePayloadJson.trim() === ""
                        ? {}
                        : JSON.parse(samplePayloadJson);
                    setSamplePayloadErr(null);
                } catch (e) {
                    setSamplePayloadErr((e as Error).message);
                    setTestRunBusy(false);
                    return;
                }
                body = {
                    sample_event: {
                        kind: parsedDef.def.trigger.kind,
                        source: "test-run",
                        payload,
                        envelope: buildSampleEnvelope(
                            sampleOriginKind,
                            sampleIdentityKind,
                            sampleIdentityTrust,
                        ),
                    },
                    client_run_id: clientRunId,
                };
            }
            const result = await testRunAutomation(id, body, token);
            setTestRunResult(result);
        } catch (e) {
            setError((e as Error).message || "Test run failed");
        } finally {
            setTestRunBusy(false);
        }
    }, [
        id,
        isNew,
        parsedDef.def,
        selectedEventId,
        token,
        samplePayloadJson,
        sampleOriginKind,
        sampleIdentityKind,
        sampleIdentityTrust,
    ]);

    const onDelete = useCallback(async () => {
        if (isNew) return;
        if (isDefault) {
            // Belt-and-suspenders — the button is hidden when
            // isDefault, but a stale-state click would otherwise hit
            // the server and get a 403. Surface the same message
            // the server returns without round-tripping.
            setError(
                `Default flows shipped by '${source}' cannot be deleted. Disable via the Enabled switch, or uninstall the source ${
                    source === "core" ? "feature" : "plugin"
                }.`,
            );
            return;
        }
        if (!confirm("Delete this flow? This cannot be undone.")) return;
        setBusy(true);
        try {
            await deleteAutomation(id, token);
            navigate("/automations", { replace: true });
        } catch (e) {
            setError((e as Error).message || "Delete failed");
            setBusy(false);
        }
    }, [id, isNew, isDefault, source, navigate, token]);

    if (!loaded) {
        return (
            <div className="execlaw-muted small p-3" data-testid="detail-loading">
                Loading…
            </div>
        );
    }

    return (
        <div className="execlaw-automation-detail" data-testid="automation-detail">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

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
                    {!isNew && !isDefault && (
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
                    {!isNew && isDefault && (
                        <span
                            className="badge text-bg-secondary align-self-center"
                            title={`This is a default flow shipped by '${source}'. Disable it via the Enabled switch instead, or uninstall the source ${
                                source === "core" ? "feature" : "plugin"
                            } to remove it.`}
                            data-testid="automation-default-badge"
                        >
                            <i className="bi bi-shield-lock-fill me-1" aria-hidden />
                            {source === "core" ? "Core default" : `From ${source}`}
                        </span>
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
                    liveTrace={liveTrace}
                    samplePayloadJson={samplePayloadJson}
                    onSamplePayloadJsonChange={setSamplePayloadJson}
                    samplePayloadErr={samplePayloadErr}
                    sampleOriginKind={sampleOriginKind}
                    onSampleOriginKindChange={setSampleOriginKind}
                    sampleIdentityKind={sampleIdentityKind}
                    onSampleIdentityKindChange={setSampleIdentityKind}
                    sampleIdentityTrust={sampleIdentityTrust}
                    onSampleIdentityTrustChange={setSampleIdentityTrust}
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
    /** Live SSE trace events for the in-flight run. */
    liveTrace: LiveTraceEvent[];
    // Synthesize-mode envelope override fields (audit fix #9).
    samplePayloadJson: string;
    onSamplePayloadJsonChange: (s: string) => void;
    samplePayloadErr: string | null;
    sampleOriginKind: string;
    onSampleOriginKindChange: (s: string) => void;
    sampleIdentityKind: string;
    onSampleIdentityKindChange: (s: string) => void;
    sampleIdentityTrust: string;
    onSampleIdentityTrustChange: (s: string) => void;
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
    liveTrace,
    samplePayloadJson,
    onSamplePayloadJsonChange,
    samplePayloadErr,
    sampleOriginKind,
    onSampleOriginKindChange,
    sampleIdentityKind,
    onSampleIdentityKindChange,
    sampleIdentityTrust,
    onSampleIdentityTrustChange,
}: TestRunDrawerProps) {
    // The envelope-builder block is only useful in synthesize mode
    // (selectedEventId === "") because real captured events already
    // carry their own envelope.
    const synthesizeMode = selectedEventId === "";
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
                            "Synthesize" runs against an envelope you compose
                            below — useful for testing trigger filters that gate
                            on origin / identity / payload.
                        </div>
                    </Form.Group>
                    {synthesizeMode && (
                        <div
                            className="border rounded p-2 mb-2"
                            style={{ background: "#0e131a", borderColor: "#30363d" }}
                            data-testid="test-run-synthesize-block"
                        >
                            <div className="small text-muted mb-1">
                                Synthesize event details
                            </div>
                            <Form.Group className="mb-2">
                                <Form.Label className="small text-muted mb-1">
                                    Payload (JSON)
                                </Form.Label>
                                <Form.Control
                                    as="textarea"
                                    rows={3}
                                    size="sm"
                                    value={samplePayloadJson}
                                    onChange={(e) =>
                                        onSamplePayloadJsonChange(e.target.value)
                                    }
                                    spellCheck={false}
                                    className="font-monospace small"
                                    data-testid="test-run-payload"
                                />
                                {samplePayloadErr && (
                                    <div
                                        className="small text-danger mt-1"
                                        data-testid="test-run-payload-error"
                                    >
                                        {samplePayloadErr}
                                    </div>
                                )}
                            </Form.Group>
                            <div className="row g-2">
                                <Form.Group className="col">
                                    <Form.Label className="small text-muted mb-1">
                                        Origin kind
                                    </Form.Label>
                                    <Form.Select
                                        size="sm"
                                        value={sampleOriginKind}
                                        onChange={(e) =>
                                            onSampleOriginKindChange(e.target.value)
                                        }
                                        data-testid="test-run-origin-kind"
                                    >
                                        <option value="system_internal">
                                            system_internal (default)
                                        </option>
                                        <option value="web_socket_session">
                                            web_socket_session
                                        </option>
                                        <option value="chat_append">chat_append</option>
                                        <option value="alert">alert</option>
                                        <option value="none">none</option>
                                    </Form.Select>
                                </Form.Group>
                                <Form.Group className="col">
                                    <Form.Label className="small text-muted mb-1">
                                        Identity kind
                                    </Form.Label>
                                    <Form.Select
                                        size="sm"
                                        value={sampleIdentityKind}
                                        onChange={(e) =>
                                            onSampleIdentityKindChange(e.target.value)
                                        }
                                        data-testid="test-run-identity-kind"
                                    >
                                        <option value="system">system</option>
                                        <option value="principal">principal</option>
                                        <option value="external">external</option>
                                    </Form.Select>
                                </Form.Group>
                                <Form.Group className="col">
                                    <Form.Label className="small text-muted mb-1">
                                        Trust
                                    </Form.Label>
                                    <Form.Select
                                        size="sm"
                                        value={sampleIdentityTrust}
                                        onChange={(e) =>
                                            onSampleIdentityTrustChange(e.target.value)
                                        }
                                        disabled={sampleIdentityKind === "system"}
                                        data-testid="test-run-identity-trust"
                                    >
                                        <option value="controller">controller</option>
                                        <option value="known_high">known_high</option>
                                        <option value="known_limited">
                                            known_limited
                                        </option>
                                        <option value="cold_contact">cold_contact</option>
                                        <option value="blocked">blocked</option>
                                    </Form.Select>
                                </Form.Group>
                            </div>
                            <div className="small text-muted mt-1">
                                The envelope is reachable from Rhai as{" "}
                                <code>event.envelope.origin.kind</code> and{" "}
                                <code>event.envelope.identity.trust</code>.
                                System identity ignores the trust field.
                            </div>
                        </div>
                    )}
                    <Button
                        variant="primary"
                        size="sm"
                        disabled={busy}
                        onClick={onRun}
                        data-testid="test-run-go"
                    >
                        {busy ? "Running…" : "Run"}
                    </Button>
                    {liveTrace.length > 0 && (
                        <LiveTraceView trace={liveTrace} runningStill={busy} />
                    )}
                    {result && <TestRunResultView result={result} />}
                </div>
            )}
        </section>
    );
}

function LiveTraceView({
    trace,
    runningStill,
}: {
    trace: LiveTraceEvent[];
    runningStill: boolean;
}) {
    // Aggregate AgentTextDelta frames per (run_id, node_id) so the
    // running agent text is shown as one growing block rather than
    // 60+ rows. Other event kinds render verbatim.
    type Row =
        | { kind: "event"; ts: number; t: string; data: Record<string, unknown> }
        | { kind: "agent-text"; nodeId: string; text: string };
    const rows: Row[] = useMemo(() => {
        const out: Row[] = [];
        for (const ev of trace) {
            if (ev.t === "agent_text_delta") {
                const nodeId =
                    (ev.data.node_id as string | undefined) ?? "(unknown)";
                const delta = (ev.data.delta as string | undefined) ?? "";
                const last = out[out.length - 1];
                if (
                    last &&
                    last.kind === "agent-text" &&
                    last.nodeId === nodeId
                ) {
                    last.text += delta;
                } else {
                    out.push({ kind: "agent-text", nodeId, text: delta });
                }
            } else {
                out.push({
                    kind: "event",
                    ts: ev.ts,
                    t: ev.t,
                    data: ev.data,
                });
            }
        }
        return out;
    }, [trace]);
    return (
        <div
            className="mt-3 border rounded p-2"
            style={{ background: "#0e131a", borderColor: "#30363d" }}
            data-testid="live-trace"
        >
            <div className="small text-muted mb-1">
                <i
                    className={`bi ${
                        runningStill ? "bi-broadcast" : "bi-check-circle"
                    } me-1`}
                    aria-hidden
                />
                Live trace · {trace.length} event(s){" "}
                {runningStill && (
                    <span className="text-info"> · streaming…</span>
                )}
            </div>
            <ul
                className="small mb-0 font-monospace"
                style={{ listStyle: "none", paddingLeft: 0, maxHeight: 200, overflowY: "auto" }}
            >
                {rows.map((r, i) => {
                    if (r.kind === "agent-text") {
                        return (
                            <li
                                key={`a-${i}`}
                                style={{ color: "#a5d8ff" }}
                                data-testid="live-trace-agent-text"
                            >
                                <span style={{ color: "#7d8590" }}>
                                    [agent_text {r.nodeId}]
                                </span>{" "}
                                {r.text}
                            </li>
                        );
                    }
                    return (
                        <li
                            key={`e-${i}`}
                            data-testid={`live-trace-row-${r.t}`}
                        >
                            <span style={{ color: "#7d8590" }}>
                                [{r.t}]
                            </span>{" "}
                            {summarizeEventData(r.t, r.data)}
                        </li>
                    );
                })}
            </ul>
        </div>
    );
}

function summarizeEventData(t: string, data: Record<string, unknown>): string {
    // One-line summary per FlowChannelEvent variant. Keeps the trace
    // readable — the raw JSON is debug-only and would explode the
    // viewport on a chatty run.
    const node = (data.node_id as string | undefined) ?? "";
    switch (t) {
        case "node_started":
            return `→ ${node}`;
        case "node_finished":
            return `✓ ${node} (${(data.outcome as string | undefined) ?? "?"})`;
        case "agent_turn_started":
            return `agent ${node} turn ${data.turn_index ?? "?"}`;
        case "agent_turn_finished":
            return `agent ${node} done (${(data.exit_tool as string | undefined) ?? "?"})`;
        case "agent_tool_call_delta":
            return `agent ${node} tool ${(data.tool_name as string | undefined) ?? "?"}`;
        case "reply_routed":
            return `reply (${(data.route as string | undefined) ?? "?"})`;
        case "run_finished":
            return `outcome=${(data.outcome as string | undefined) ?? "?"}`;
        default:
            return JSON.stringify(data);
    }
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
