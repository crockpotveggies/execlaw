// /automations landing page (M4b).
//
// Four metric cards across the top (Active, Runs 24h, Success rate,
// Untriaged), then a suggestions section (when the daily sweep has
// produced any), then the master automations list. Clicking a row
// opens `/automations/:id`; clicking "+ New automation" opens the
// detail page with a fresh definition.
//
// The visual canvas editor (React Flow) is a follow-up — for now the
// detail page is JSON-edit-only. The design doc's "JSON view round-
// trips 100% with the canvas" path means this is forward-compatible.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import { Link, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { ZeroState } from "../components/ZeroState";
import {
    actionSuggestion,
    dismissSuggestion,
    formatPercent,
    getAutomationMetrics,
    kindLabel,
    listAutomations,
    listSuggestions,
    setAutomationEnabled,
    type AutomationMetrics,
    type AutomationView,
    type SuggestionView,
} from "../api/automations";

export function AutomationsPage() {
    const auth = useAuth();
    const token = auth.getAccessToken;
    const navigate = useNavigate();

    const [metrics, setMetrics] = useState<AutomationMetrics | null>(null);
    const [automations, setAutomations] = useState<AutomationView[]>([]);
    const [suggestions, setSuggestions] = useState<SuggestionView[]>([]);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        setError(null);
        try {
            const [m, a, s] = await Promise.all([
                getAutomationMetrics(token),
                listAutomations(token),
                listSuggestions(token),
            ]);
            setMetrics(m);
            setAutomations(a);
            setSuggestions(s);
        } catch (e) {
            setError((e as Error).message || "Failed to load flows");
        }
    }, [token]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onToggle = useCallback(
        async (a: AutomationView) => {
            setBusyId(a.id);
            try {
                await setAutomationEnabled(a.id, !a.enabled, token);
                await refresh();
            } catch (e) {
                setError((e as Error).message);
            } finally {
                setBusyId(null);
            }
        },
        [token, refresh],
    );

    const onDismissSuggestion = useCallback(
        async (s: SuggestionView) => {
            setBusyId(s.id);
            try {
                await dismissSuggestion(s.id, token);
                await refresh();
            } catch (e) {
                setError((e as Error).message);
            } finally {
                setBusyId(null);
            }
        },
        [token, refresh],
    );

    const onActionSuggestion = useCallback(
        async (s: SuggestionView) => {
            // Optimistic: mark actioned server-side, then navigate to a
            // fresh detail page seeded with the suggestion's kind. The
            // detail page reads `?suggestion=<id>` to know it's coming
            // from a suggestion (M5 hook).
            try {
                await actionSuggestion(s.id, token);
            } catch (e) {
                setError((e as Error).message);
                return;
            }
            navigate(`/automations/new?suggestion=${encodeURIComponent(s.id)}&kind=${encodeURIComponent(s.kind)}&source=${encodeURIComponent(s.source)}`);
        },
        [token, navigate],
    );

    return (
        <div data-testid="automations-landing">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            <div className="d-flex justify-content-between align-items-start gap-3 mb-3">
                <p className="execlaw-muted small mb-0">
                    Event-triggered flows. Webhooks, routines, and plugin
                    emits land on the bus; matching flows run their
                    graph and write a per-step trace you can replay.
                </p>
                <Link
                    to="/automations/new"
                    className="btn btn-primary btn-sm flex-shrink-0"
                    data-testid="new-automation-btn"
                >
                    <i className="bi bi-plus-lg me-1" aria-hidden /> New flow
                </Link>
            </div>

            <MetricCards metrics={metrics} />

            {suggestions.length > 0 && (
                <SuggestionsSection
                    suggestions={suggestions}
                    busyId={busyId}
                    onAction={onActionSuggestion}
                    onDismiss={onDismissSuggestion}
                />
            )}

            <h3 className="h6 mt-4 mb-2">Your flows</h3>
            {automations.length === 0 ? (
                <ZeroState
                    testId="automations-empty"
                    icon="bi-lightning-charge-fill"
                    title={<>No flows yet.</>}
                    description={
                        <>
                            Event-triggered flows fire when webhooks,
                            routines, or plugins land matching events on
                            the bus — each run writes a per-step trace
                            you can replay. Start from a suggestion above,
                            or build one from scratch.
                        </>
                    }
                    action={
                        <Link
                            to="/automations/new"
                            className="btn btn-sm btn-primary"
                            data-testid="automations-empty-new-btn"
                        >
                            <i className="bi bi-plus-lg me-1" aria-hidden />
                            New flow
                        </Link>
                    }
                />
            ) : (
                <AutomationsTable
                    automations={automations}
                    busyId={busyId}
                    onToggle={onToggle}
                />
            )}
        </div>
    );
}

function MetricCards({ metrics }: { metrics: AutomationMetrics | null }) {
    const cards = [
        {
            label: "Active",
            value: metrics?.active_count ?? "—",
            help: "Enabled flows",
            testId: "metric-active",
        },
        {
            label: "Runs (24h)",
            value: metrics?.runs_24h ?? "—",
            help: "Total executions in the last 24 hours",
            testId: "metric-runs",
        },
        {
            label: "Success rate (24h)",
            value: formatPercent(metrics?.success_rate_24h ?? null),
            help: "Successful runs as a fraction of (success + failed)",
            testId: "metric-success",
        },
        {
            label: "Untriaged (24h)",
            value: metrics?.untriaged_kinds_24h ?? "—",
            help: "Distinct event kinds with no matching flow",
            testId: "metric-untriaged",
        },
    ];
    return (
        <div
            className="row row-cols-1 row-cols-md-4 g-2"
            data-testid="metric-cards"
        >
            {cards.map((c) => (
                <div key={c.label} className="col">
                    <div
                        className="execlaw-card h-100 mb-0"
                        data-testid={c.testId}
                    >
                        <div className="execlaw-muted small">{c.label}</div>
                        <div className="h4 mb-0">{c.value}</div>
                        <div className="execlaw-muted small mt-1">{c.help}</div>
                    </div>
                </div>
            ))}
        </div>
    );
}

interface SuggestionsSectionProps {
    suggestions: SuggestionView[];
    busyId: string | null;
    onAction: (s: SuggestionView) => void;
    onDismiss: (s: SuggestionView) => void;
}

function SuggestionsSection({
    suggestions,
    busyId,
    onAction,
    onDismiss,
}: SuggestionsSectionProps) {
    return (
        <section className="mt-4" data-testid="suggestions-section">
            <h3 className="h6 mb-2">
                <i className="bi bi-lightbulb me-1" aria-hidden />
                Suggestions ({suggestions.length})
            </h3>
            <ul className="list-unstyled mb-0">
                {suggestions.map((s) => (
                    <li
                        key={s.id}
                        className="execlaw-card d-flex justify-content-between align-items-center"
                        data-testid={`suggestion-${s.id}`}
                    >
                        <div className="me-3">
                            <div className="small execlaw-muted">
                                {s.event_count} {kindLabel(s.kind).toLowerCase()} events
                                from <code>{s.source}</code> with no flow
                            </div>
                            <div className="fw-semibold">
                                {s.suggested_name}
                            </div>
                        </div>
                        <div className="d-flex gap-2">
                            <Button
                                variant="primary"
                                size="sm"
                                disabled={busyId === s.id}
                                onClick={() => onAction(s)}
                                data-testid={`suggestion-${s.id}-review`}
                            >
                                Review and create
                            </Button>
                            <Button
                                variant="outline-secondary"
                                size="sm"
                                disabled={busyId === s.id}
                                onClick={() => onDismiss(s)}
                                data-testid={`suggestion-${s.id}-dismiss`}
                            >
                                Skip
                            </Button>
                        </div>
                    </li>
                ))}
            </ul>
        </section>
    );
}

interface AutomationsTableProps {
    automations: AutomationView[];
    busyId: string | null;
    onToggle: (a: AutomationView) => void;
}

function fmtTs(ts: number): string {
    return new Date(ts * 1000).toLocaleString();
}

function AutomationsTable({
    automations,
    busyId,
    onToggle,
}: AutomationsTableProps) {
    return (
        <table
            className="table table-sm align-middle"
            data-testid="automations-table"
        >
            <thead>
                <tr>
                    <th>Name</th>
                    <th>Trigger</th>
                    <th>Updated</th>
                    <th>Status</th>
                    <th aria-label="actions" />
                </tr>
            </thead>
            <tbody>
                {automations.map((a) => (
                    <tr key={a.id} data-testid={`automation-row-${a.id}`}>
                        <td>
                            <Link to={`/automations/${a.id}`}>{a.name}</Link>
                        </td>
                        <td>
                            <code>{kindLabel(a.definition.trigger.kind)}</code>
                        </td>
                        <td className="text-muted small">{fmtTs(a.updated_at)}</td>
                        <td>
                            {a.enabled ? (
                                <span className="badge bg-success-subtle text-success-emphasis">
                                    Enabled
                                </span>
                            ) : (
                                <span className="badge bg-secondary-subtle text-secondary-emphasis">
                                    Disabled
                                </span>
                            )}
                        </td>
                        <td className="text-end">
                            <Button
                                variant="outline-secondary"
                                size="sm"
                                disabled={busyId === a.id}
                                onClick={() => onToggle(a)}
                                data-testid={`automation-${a.id}-toggle`}
                            >
                                {a.enabled ? "Disable" : "Enable"}
                            </Button>
                        </td>
                    </tr>
                ))}
            </tbody>
        </table>
    );
}
