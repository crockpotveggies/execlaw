// /research — operator drill-down (C6).
//
// Two-pane layout: a left list of every research job (newest first,
// status pill, query truncated) + a right detail pane showing the
// selected job's plan tree, gather notes, workspace path, and the
// final report markdown when complete.
//
// Polls every 5s while at least one job is non-terminal so the
// operator sees status flips without manually refreshing. Cards
// arriving over the WS event bus update the same chat-pane store
// (via the existing Chat.tsx wiring); this page polls a separate
// admin endpoint that always carries the canonical row.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Navigate, useNavigate, useParams } from "react-router-dom";
import {
    advanceResearchJob,
    cancelResearchJob,
    getResearchReport,
    listResearchJobs,
    RESEARCH_TERMINAL_STATUSES,
    type ResearchJobSummaryView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { setActiveThread } from "../chat/store";
import { ErrorBanner } from "../components/ErrorBanner";

const POLL_INTERVAL_MS = 5_000;

type StatusFilter = "all" | "active" | "complete" | "failed";

export function Research() {
    const auth = useAuth();
    const navigate = useNavigate();
    const params = useParams<{ jobId?: string }>();
    const getToken = auth.getAccessToken;
    const [jobs, setJobs] = useState<ResearchJobSummaryView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [filter, setFilter] = useState<StatusFilter>("all");

    const meRole = auth.user?.role ?? "viewer";
    const canView = meRole === "controller";

    const refresh = useCallback(async () => {
        try {
            const r = await listResearchJobs(getToken);
            setJobs(r.jobs);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        if (auth.status !== "authenticated" || !canView) return;
        void refresh();
    }, [auth.status, canView, refresh]);

    /// Poll while at least one job is non-terminal; pause polling
    /// once everything has settled so an idle SPA doesn't burn
    /// network. The poll restarts whenever the list mutates a job
    /// out of terminal back to active (impossible today; defensive).
    const hasActive = useMemo(
        () =>
            (jobs ?? []).some(
                (j) => !RESEARCH_TERMINAL_STATUSES.has(j.status),
            ),
        [jobs],
    );

    useEffect(() => {
        if (!hasActive || !canView) return;
        const id = window.setInterval(() => {
            void refresh();
        }, POLL_INTERVAL_MS);
        return () => window.clearInterval(id);
    }, [hasActive, canView, refresh]);

    const onNewThread = useCallback(() => {
        setActiveThread(null);
        navigate("/chat");
    }, [navigate]);

    const onSignOut = useCallback(() => {
        void auth.signOut();
    }, [auth]);

    if (auth.status === "loading") {
        return (
            <div className="execlaw-auth-shell">
                <div className="execlaw-muted small">Loading session…</div>
            </div>
        );
    }
    if (auth.status === "unauthenticated") {
        return <Navigate to="/login" replace />;
    }

    return (
        <div className="execlaw-shell">
            <Sidebar onNewThread={onNewThread} onSignOut={onSignOut} />
            <main className="execlaw-main">
                <header className="execlaw-main__head">
                    <h2 className="h6 mb-0">
                        <i className="bi bi-binoculars me-2" aria-hidden />
                        Research
                    </h2>
                </header>
                <div
                    className="execlaw-page execlaw-research"
                    data-testid="research-page"
                >
                    <ErrorBanner
                        message={error}
                        onDismiss={() => setError(null)}
                        className="m-3"
                    />
                    {!canView ? (
                        <div className="m-3 execlaw-muted small">
                            Read-only research surface is Controller-only
                            today. Ask an operator with the Controller role
                            to drill into research jobs for you.
                        </div>
                    ) : jobs === null ? (
                        <div className="m-3 execlaw-muted small">Loading…</div>
                    ) : jobs.length === 0 ? (
                        <div className="m-3 execlaw-muted small">
                            No research jobs yet. Ask the agent to{" "}
                            <code>research_start</code> a question, or fire
                            one from any chat thread.
                        </div>
                    ) : (
                        <ResearchTwoPane
                            jobs={jobs}
                            selectedId={params.jobId ?? null}
                            onSelect={(id) =>
                                navigate(`/research/${encodeURIComponent(id)}`)
                            }
                            filter={filter}
                            onFilter={setFilter}
                        />
                    )}
                </div>
            </main>
        </div>
    );
}

function ResearchTwoPane({
    jobs,
    selectedId,
    onSelect,
    filter,
    onFilter,
}: {
    jobs: ResearchJobSummaryView[];
    selectedId: string | null;
    onSelect: (id: string) => void;
    filter: StatusFilter;
    onFilter: (f: StatusFilter) => void;
}) {
    const filtered = useMemo(
        () => jobs.filter((j) => matchesFilter(j, filter)),
        [jobs, filter],
    );
    const selected = useMemo(
        () => jobs.find((j) => j.id === selectedId) ?? null,
        [jobs, selectedId],
    );

    const counts = useMemo(() => {
        const c = { all: jobs.length, active: 0, complete: 0, failed: 0 };
        for (const j of jobs) {
            if (!RESEARCH_TERMINAL_STATUSES.has(j.status)) c.active += 1;
            else if (j.status === "complete") c.complete += 1;
            else if (j.status === "failed") c.failed += 1;
        }
        return c;
    }, [jobs]);

    return (
        <>
            <div className="d-flex align-items-center gap-3 px-3 py-2 flex-wrap">
                <span className="execlaw-muted small">
                    {filtered.length} of {jobs.length} job
                    {jobs.length === 1 ? "" : "s"}
                </span>
                <div
                    className="execlaw-research__filters"
                    role="tablist"
                    aria-label="Filter research jobs"
                >
                    {(
                        [
                            ["all", "All", counts.all],
                            ["active", "Active", counts.active],
                            ["complete", "Complete", counts.complete],
                            ["failed", "Failed", counts.failed],
                        ] as const
                    ).map(([key, label, n]) => (
                        <button
                            key={key}
                            type="button"
                            role="tab"
                            aria-selected={filter === key}
                            className={
                                "execlaw-research__filter" +
                                (filter === key ? " is-active" : "")
                            }
                            onClick={() => onFilter(key)}
                            data-testid={`research-filter-${key}`}
                        >
                            {label}
                            <span className="execlaw-research__filter-count">
                                {n}
                            </span>
                        </button>
                    ))}
                </div>
            </div>
            <div
                className="execlaw-research__split d-flex flex-grow-1"
                style={{ minHeight: 0 }}
            >
                <aside
                    className="execlaw-research__list"
                    data-testid="research-list"
                >
                    {filtered.length === 0 ? (
                        <div className="m-3 execlaw-muted small">
                            No jobs match this filter.
                        </div>
                    ) : (
                        filtered.map((j) => (
                            <button
                                key={j.id}
                                type="button"
                                className={
                                    "execlaw-research__list-row" +
                                    (j.id === selectedId ? " is-active" : "")
                                }
                                onClick={() => onSelect(j.id)}
                                data-testid="research-list-row"
                                data-status={j.status}
                            >
                                <div className="execlaw-research__list-row-head">
                                    <span
                                        className={`badge bg-${badgeColor(j.status)}`}
                                    >
                                        {j.status}
                                    </span>
                                    <span className="execlaw-research__list-time execlaw-muted small">
                                        {formatRelativeTime(j.updated_at)}
                                    </span>
                                </div>
                                <div className="execlaw-research__list-query">
                                    {j.query}
                                </div>
                            </button>
                        ))
                    )}
                </aside>
                <section
                    className="execlaw-research__detail"
                    data-testid="research-detail"
                >
                    {selected === null ? (
                        <div className="execlaw-research__detail-empty execlaw-muted">
                            <i
                                className="bi bi-binoculars d-block mb-2"
                                style={{ fontSize: "2rem" }}
                                aria-hidden
                            />
                            <div>Select a job on the left to view its plan and report.</div>
                        </div>
                    ) : (
                        <ResearchJobDetail job={selected} />
                    )}
                </section>
            </div>
        </>
    );
}

function matchesFilter(
    j: ResearchJobSummaryView,
    f: StatusFilter,
): boolean {
    if (f === "all") return true;
    if (f === "active") return !RESEARCH_TERMINAL_STATUSES.has(j.status);
    if (f === "complete") return j.status === "complete";
    if (f === "failed") return j.status === "failed";
    return true;
}

function badgeColor(status: ResearchJobSummaryView["status"]): string {
    switch (status) {
        case "complete":
            return "success";
        case "failed":
            return "danger";
        case "cancelled":
            return "secondary";
        case "pending":
        case "planning":
            return "info";
        case "planned":
            return "primary";
        case "gathering":
        case "synthesizing":
            return "warning";
    }
}

function ResearchJobDetail({ job }: { job: ResearchJobSummaryView }) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [report, setReport] = useState<string | null>(null);
    const [reportLoaded, setReportLoaded] = useState(false);
    const [busy, setBusy] = useState<"advance" | "cancel" | null>(null);
    const [actionError, setActionError] = useState<string | null>(null);

    /// `Planned` and `Gathering` rows have an Approve button; any
    /// non-terminal row has a Cancel button. The Approve copy
    /// switches between "Run gather" / "Run synthesize" based on
    /// the prior status so the operator's mental model stays clear.
    const canAdvance =
        job.status === "planned" || job.status === "gathering";
    const canCancel = !RESEARCH_TERMINAL_STATUSES.has(job.status);

    const onAdvance = useCallback(async () => {
        setBusy("advance");
        setActionError(null);
        try {
            await advanceResearchJob(job.id, getToken);
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(null);
        }
    }, [job.id, getToken]);

    const onCancel = useCallback(async () => {
        // Native confirm — terminal action, worth the friction.
        if (!window.confirm(`Cancel research job?\n\n${job.query}`)) return;
        setBusy("cancel");
        setActionError(null);
        try {
            await cancelResearchJob(job.id, getToken, "operator cancelled");
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(null);
        }
    }, [job.id, job.query, getToken]);

    useEffect(() => {
        // Only fetch the report when the job has actually completed —
        // otherwise the endpoint returns null and we'd burn a round-
        // trip per poll.
        if (job.status !== "complete") {
            setReport(null);
            setReportLoaded(false);
            return;
        }
        let cancelled = false;
        (async () => {
            try {
                const r = await getResearchReport(job.id, getToken);
                if (!cancelled) {
                    setReport(r.report_markdown);
                    setReportLoaded(true);
                }
            } catch {
                if (!cancelled) {
                    setReport(null);
                    setReportLoaded(true);
                }
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [job.id, job.status, getToken]);

    const doneCount = job.notes.filter((n) => n.state === "Done").length;
    const failedCount = job.notes.filter((n) => n.state === "Failed").length;
    const totalSteps = job.plan?.steps.length ?? job.notes.length;

    return (
        <div
            className="execlaw-research__detail-body"
            data-testid="research-detail-body"
        >
            <header className="execlaw-research__detail-head">
                <div className="d-flex align-items-center gap-2 flex-wrap mb-2">
                    <span className={`badge bg-${badgeColor(job.status)}`}>
                        {job.status}
                    </span>
                    {totalSteps > 0 && (
                        <span className="execlaw-muted small">
                            {doneCount}/{totalSteps} steps done
                            {failedCount > 0 && ` · ${failedCount} failed`}
                        </span>
                    )}
                    <div className="ms-auto d-flex gap-2">
                        {canAdvance && (
                            <button
                                type="button"
                                className="btn btn-primary btn-sm"
                                disabled={busy !== null}
                                onClick={() => void onAdvance()}
                                data-testid="research-detail-advance"
                            >
                                {busy === "advance"
                                    ? "Approving…"
                                    : job.status === "planned"
                                      ? "Approve · run gather"
                                      : "Approve · run synthesize"}
                            </button>
                        )}
                        {canCancel && (
                            <button
                                type="button"
                                className="btn btn-outline-danger btn-sm"
                                disabled={busy !== null}
                                onClick={() => void onCancel()}
                                data-testid="research-detail-cancel"
                            >
                                {busy === "cancel" ? "Cancelling…" : "Cancel"}
                            </button>
                        )}
                    </div>
                </div>
                <h3 className="execlaw-research__detail-title">{job.query}</h3>
                <div className="execlaw-muted small mt-1">
                    Job <code>{job.id}</code> · conversation{" "}
                    <code>{job.conversation_id}</code>
                </div>
            </header>

            {actionError && (
                <ErrorBanner
                    message={actionError}
                    onDismiss={() => setActionError(null)}
                    className="mb-3"
                />
            )}
            {job.error && (
                <div
                    className="execlaw-card-task__error mb-3"
                    data-testid="research-detail-error"
                >
                    {job.error}
                </div>
            )}

            {job.plan && (
                <section
                    className="execlaw-research__section"
                    data-testid="research-detail-plan"
                >
                    <h4 className="execlaw-research__section-title">Plan</h4>
                    <div className="execlaw-research__thesis">
                        <span className="execlaw-muted small me-2">Thesis:</span>
                        {job.plan.thesis}
                    </div>
                    <ol className="execlaw-research__steps">
                        {job.plan.steps.map((s, i) => {
                            const note = job.notes.find((n) => n.index === i);
                            const state = note?.state ?? "Pending";
                            return (
                                <li
                                    key={i}
                                    className="execlaw-research__step"
                                    data-testid="research-detail-step"
                                    data-state={state}
                                >
                                    <div className="execlaw-research__step-head">
                                        <span
                                            className={`badge bg-${noteColor(state)}`}
                                        >
                                            {state}
                                        </span>
                                        <span className="execlaw-research__step-query">
                                            {s.query}
                                        </span>
                                    </div>
                                    {s.rationale && (
                                        <div className="execlaw-muted small execlaw-research__step-rationale">
                                            {s.rationale}
                                        </div>
                                    )}
                                    {note?.excerpt && (
                                        <div className="execlaw-research__step-excerpt">
                                            {note.excerpt}
                                        </div>
                                    )}
                                    {note?.sources && note.sources.length > 0 && (
                                        <ul className="execlaw-research__sources">
                                            {note.sources.map((src, j) => (
                                                <li key={`${src.url}-${j}`}>
                                                    {src.fetched_ok === false ? (
                                                        <span className="execlaw-muted">
                                                            ✗ {src.url}
                                                            {src.error &&
                                                                ` — ${src.error}`}
                                                        </span>
                                                    ) : (
                                                        <a
                                                            href={src.url}
                                                            target="_blank"
                                                            rel="noopener noreferrer"
                                                        >
                                                            {src.title ?? src.url}
                                                        </a>
                                                    )}
                                                </li>
                                            ))}
                                        </ul>
                                    )}
                                </li>
                            );
                        })}
                    </ol>
                </section>
            )}

            {job.workspace_path && (
                <section
                    className="execlaw-research__section"
                    data-testid="research-detail-workspace"
                >
                    <h4 className="execlaw-research__section-title">
                        Workspace
                    </h4>
                    <code className="execlaw-muted small">
                        {job.workspace_path}
                    </code>
                </section>
            )}

            {job.status === "complete" && (
                <section
                    className="execlaw-research__section"
                    data-testid="research-detail-report"
                >
                    <h4 className="execlaw-research__section-title">Report</h4>
                    {!reportLoaded ? (
                        <div className="execlaw-muted small">Loading…</div>
                    ) : report === null ? (
                        <div className="execlaw-muted small">
                            No report on disk for this job (workspace may
                            have been retention-purged).
                        </div>
                    ) : (
                        <pre
                            className="execlaw-research__report"
                            data-testid="research-detail-report-body"
                        >
                            {report}
                        </pre>
                    )}
                </section>
            )}
        </div>
    );
}

function noteColor(state: string): string {
    switch (state) {
        case "Done":
            return "success";
        case "Running":
            return "primary";
        case "Failed":
            return "danger";
        default:
            return "secondary";
    }
}

function formatRelativeTime(unixSecs: number): string {
    const deltaSec = Math.floor(Date.now() / 1000) - unixSecs;
    if (deltaSec < 60) return "just now";
    if (deltaSec < 3600) return `${Math.floor(deltaSec / 60)}m ago`;
    if (deltaSec < 86_400) return `${Math.floor(deltaSec / 3600)}h ago`;
    return `${Math.floor(deltaSec / 86_400)}d ago`;
}
