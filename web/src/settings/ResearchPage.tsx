// Settings → Research (C3 — deep-research subsystem).
//
// Operator-editable defaults for the research subsystem: timeouts,
// parallelism, sub-query caps, phase-gate behaviour, and the default
// search provider. Per-conversation and per-job overrides apply on
// top of these in C4-C5.
//
// All values are validated server-side; the page surfaces 400
// responses verbatim. Read-only when the operator isn't a Controller.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    getResearchSettings,
    updateResearchSettings,
    RESEARCH_PHASE_GATE_OPTIONS,
    type ResearchSettings,
    type ResearchPhaseGates,
    type UpdateResearchSettingsRequest,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

export function ResearchPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [settings, setSettings] = useState<ResearchSettings | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [maxWallClockMinutes, setMaxWallClockMinutes] = useState(30);
    const [maxTotalTokens, setMaxTotalTokens] = useState(100_000);
    const [maxSubqueries, setMaxSubqueries] = useState(12);
    const [parallelWorkers, setParallelWorkers] = useState(3);
    const [maxUrlsPerSubquery, setMaxUrlsPerSubquery] = useState(5);
    const [maxPagesTotal, setMaxPagesTotal] = useState(60);
    const [autoCancelAfterIdleSecs, setAutoCancelAfterIdleSecs] = useState(120);
    const [phaseGates, setPhaseGates] = useState<ResearchPhaseGates>("plan_only");
    /// Empty string in the input means "inherit" — sent as `null`
    /// over the wire so the column gets cleared.
    const [searchProvider, setSearchProvider] = useState("");
    const [testBusy, setTestBusy] = useState(false);
    const [testMessage, setTestMessage] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await getResearchSettings(getToken);
            setSettings(r);
            setMaxWallClockMinutes(r.max_wall_clock_minutes);
            setMaxTotalTokens(r.max_total_tokens);
            setMaxSubqueries(r.max_subqueries);
            setParallelWorkers(r.parallel_workers);
            setMaxUrlsPerSubquery(r.max_urls_per_subquery);
            setMaxPagesTotal(r.max_pages_total);
            setAutoCancelAfterIdleSecs(r.auto_cancel_after_idle_secs);
            setPhaseGates(r.phase_gates);
            setSearchProvider(r.default_search_provider ?? "");
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    const dirty =
        !!settings &&
        (settings.max_wall_clock_minutes !== maxWallClockMinutes ||
            settings.max_total_tokens !== maxTotalTokens ||
            settings.max_subqueries !== maxSubqueries ||
            settings.parallel_workers !== parallelWorkers ||
            settings.max_urls_per_subquery !== maxUrlsPerSubquery ||
            settings.max_pages_total !== maxPagesTotal ||
            settings.auto_cancel_after_idle_secs !== autoCancelAfterIdleSecs ||
            settings.phase_gates !== phaseGates ||
            (settings.default_search_provider ?? "") !== searchProvider.trim());

    const onSave = useCallback(async () => {
        if (!settings) return;
        setBusy(true);
        setError(null);
        try {
            const body: UpdateResearchSettingsRequest = {};
            if (settings.max_wall_clock_minutes !== maxWallClockMinutes) {
                body.max_wall_clock_minutes = maxWallClockMinutes;
            }
            if (settings.max_total_tokens !== maxTotalTokens) {
                body.max_total_tokens = maxTotalTokens;
            }
            if (settings.max_subqueries !== maxSubqueries) {
                body.max_subqueries = maxSubqueries;
            }
            if (settings.parallel_workers !== parallelWorkers) {
                body.parallel_workers = parallelWorkers;
            }
            if (settings.max_urls_per_subquery !== maxUrlsPerSubquery) {
                body.max_urls_per_subquery = maxUrlsPerSubquery;
            }
            if (settings.max_pages_total !== maxPagesTotal) {
                body.max_pages_total = maxPagesTotal;
            }
            if (
                settings.auto_cancel_after_idle_secs !== autoCancelAfterIdleSecs
            ) {
                body.auto_cancel_after_idle_secs = autoCancelAfterIdleSecs;
            }
            if (settings.phase_gates !== phaseGates) {
                body.phase_gates = phaseGates;
            }
            const trimmed = searchProvider.trim();
            if ((settings.default_search_provider ?? "") !== trimmed) {
                body.default_search_provider = trimmed === "" ? null : trimmed;
            }
            if (Object.keys(body).length === 0) {
                setBusy(false);
                return;
            }
            const r = await updateResearchSettings(body, getToken);
            setSettings(r);
            setMaxWallClockMinutes(r.max_wall_clock_minutes);
            setMaxTotalTokens(r.max_total_tokens);
            setMaxSubqueries(r.max_subqueries);
            setParallelWorkers(r.parallel_workers);
            setMaxUrlsPerSubquery(r.max_urls_per_subquery);
            setMaxPagesTotal(r.max_pages_total);
            setAutoCancelAfterIdleSecs(r.auto_cancel_after_idle_secs);
            setPhaseGates(r.phase_gates);
            setSearchProvider(r.default_search_provider ?? "");
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [
        settings,
        maxWallClockMinutes,
        maxTotalTokens,
        maxSubqueries,
        parallelWorkers,
        maxUrlsPerSubquery,
        maxPagesTotal,
        autoCancelAfterIdleSecs,
        phaseGates,
        searchProvider,
        getToken,
    ]);

    /// Fire a tiny canned research job to exercise the whole pipe end
    /// to end. The supervisor picks it up on the next 5s tick.
    const onTest = useCallback(async () => {
        setTestBusy(true);
        setTestMessage(null);
        try {
            // The research_start tool runs through the chat path —
            // surfacing it here as a one-click affordance would
            // require an /api/admin/research/test endpoint we
            // haven't built yet. For C3 the page just reminds the
            // operator how to fire one from the chat composer.
            setTestMessage(
                'Type "/research what\'s the weather in San Francisco today" in any chat to fire a smoke-test job.',
            );
        } finally {
            setTestBusy(false);
        }
    }, []);

    const renderNumberField = (
        id: string,
        label: string,
        helpText: string,
        value: number,
        onChange: (n: number) => void,
        min: number,
        max: number,
    ) => (
        <Form.Group className="mb-3" key={id}>
            <Form.Label className="execlaw-muted small mb-1" htmlFor={id}>
                {label}
            </Form.Label>
            <Form.Control
                id={id}
                type="number"
                min={min}
                max={max}
                value={value}
                disabled={!canMutate || busy}
                onChange={(e) => {
                    const n = Number(e.target.value);
                    if (!Number.isNaN(n)) onChange(n);
                }}
                data-testid={id}
            />
            <Form.Text className="execlaw-muted">{helpText}</Form.Text>
        </Form.Group>
    );

    return (
        <div data-testid="settings-research">
            <p className="execlaw-muted small mb-3">
                Defaults for the deep-research subsystem. The agent uses
                <code className="ms-1 me-1">research_start</code>
                to enqueue a job; the supervisor picks it up, makes a
                planner LLM call, then (in upcoming releases) fans out
                gather workers and synthesises a report. Per-job and
                per-conversation overrides apply on top of these.
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can change research
                    defaults.
                </div>
            )}

            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            {settings === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : (
                <div className="execlaw-card" data-testid="research-form">
                    <Form.Group className="mb-3">
                        <Form.Label
                            className="execlaw-muted small mb-1"
                            htmlFor="research-phase-gates"
                        >
                            Phase gates
                        </Form.Label>
                        <Form.Select
                            id="research-phase-gates"
                            value={phaseGates}
                            disabled={!canMutate || busy}
                            onChange={(e) =>
                                setPhaseGates(
                                    e.target.value as ResearchPhaseGates,
                                )
                            }
                            data-testid="research-phase-gates"
                        >
                            {RESEARCH_PHASE_GATE_OPTIONS.map((opt) => (
                                <option key={opt.value} value={opt.value}>
                                    {opt.label}
                                </option>
                            ))}
                        </Form.Select>
                        <Form.Text className="execlaw-muted">
                            {RESEARCH_PHASE_GATE_OPTIONS.find(
                                (o) => o.value === phaseGates,
                            )?.description}
                        </Form.Text>
                    </Form.Group>

                    {renderNumberField(
                        "research-max-wall-clock",
                        "Max wall-clock (minutes)",
                        "Hard kill switch. A job past this many minutes is terminated regardless of phase.",
                        maxWallClockMinutes,
                        setMaxWallClockMinutes,
                        1,
                        24 * 60,
                    )}

                    {renderNumberField(
                        "research-max-total-tokens",
                        "Max total tokens",
                        "Sum across plan + gather + synthesize. Cost ceiling.",
                        maxTotalTokens,
                        setMaxTotalTokens,
                        1,
                        10_000_000,
                    )}

                    {renderNumberField(
                        "research-max-subqueries",
                        "Max sub-queries",
                        "Planner cap on how many sub-queries the gather phase fans out.",
                        maxSubqueries,
                        setMaxSubqueries,
                        1,
                        64,
                    )}

                    {renderNumberField(
                        "research-parallel-workers",
                        "Parallel gather workers",
                        "How many sub-queries run in parallel during gather. Higher uses more bandwidth + LLM context.",
                        parallelWorkers,
                        setParallelWorkers,
                        1,
                        16,
                    )}

                    {renderNumberField(
                        "research-max-urls-per-subquery",
                        "Max URLs per sub-query",
                        "Search-result fetch cap per sub-query worker.",
                        maxUrlsPerSubquery,
                        setMaxUrlsPerSubquery,
                        1,
                        20,
                    )}

                    {renderNumberField(
                        "research-max-pages-total",
                        "Max pages total",
                        "Belt-and-braces — kills further fetches once exceeded across the whole job.",
                        maxPagesTotal,
                        setMaxPagesTotal,
                        1,
                        500,
                    )}

                    {renderNumberField(
                        "research-auto-cancel-idle",
                        "Auto-cancel after idle (seconds)",
                        "If no progress event lands in this many seconds the supervisor terminates the job.",
                        autoCancelAfterIdleSecs,
                        setAutoCancelAfterIdleSecs,
                        10,
                        3600,
                    )}

                    <Form.Group className="mb-3">
                        <Form.Label
                            className="execlaw-muted small mb-1"
                            htmlFor="research-default-search-provider"
                        >
                            Default search provider
                        </Form.Label>
                        <Form.Control
                            id="research-default-search-provider"
                            value={searchProvider}
                            disabled={!canMutate || busy}
                            onChange={(e) => setSearchProvider(e.target.value)}
                            placeholder="(inherit from Settings → Search)"
                            data-testid="research-default-search-provider"
                        />
                        <Form.Text className="execlaw-muted">
                            Provider id (e.g.{" "}
                            <code>duckduckgo</code>, <code>brave</code>). Leave
                            blank to inherit the global default from Settings →
                            Search.
                        </Form.Text>
                    </Form.Group>

                    {canMutate && (
                        <div className="d-flex gap-2 align-items-center">
                            <Button
                                variant="primary"
                                disabled={busy || !dirty}
                                onClick={() => void onSave()}
                                data-testid="research-save"
                            >
                                Save
                            </Button>
                            {dirty && (
                                <Button
                                    variant="outline-secondary"
                                    disabled={busy}
                                    onClick={() => void refresh()}
                                    data-testid="research-revert"
                                >
                                    Revert
                                </Button>
                            )}
                            <div className="ms-auto">
                                <Button
                                    variant="outline-secondary"
                                    disabled={testBusy}
                                    onClick={() => void onTest()}
                                    data-testid="research-test"
                                >
                                    Test research
                                </Button>
                            </div>
                        </div>
                    )}

                    {testMessage && (
                        <div
                            className="execlaw-muted small mt-3"
                            data-testid="research-test-hint"
                        >
                            <i className="bi bi-info-circle me-1" aria-hidden />
                            {testMessage}
                        </div>
                    )}
                </div>
            )}
        </div>
    );
}
