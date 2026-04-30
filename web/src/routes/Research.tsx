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

export function Research() {
    const auth = useAuth();
    const navigate = useNavigate();
    const params = useParams<{ jobId?: string }>();
    const getToken = auth.getAccessToken;
    const [jobs, setJobs] = useState<ResearchJobSummaryView[] | null>(null);
    const [error, setError] = useState<string | null>(null);

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
                <div className="execlaw-research" data-testid="research-page">
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
}: {
    jobs: ResearchJobSummaryView[];
    selectedId: string | null;
    onSelect: (id: string) => void;
}) {
    const selected = useMemo(
        () => jobs.find((j) => j.id === selectedId) ?? null,
        [jobs, selectedId],
    );
    return (
        <div className="execlaw-research__split">
            <aside
                className="execlaw-research__list"
                data-testid="research-list"
            >
                {jobs.map((j) => (
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
                        <span className={`badge bg-${badgeColor(j.status)} me-2`}>
                            {j.status}
                        </span>
                        <span className="execlaw-research__list-query">
                            {j.query}
                        </span>
                        <div className="execlaw-research__list-time execlaw-muted small">
                            {formatRelativeTime(j.updated_at)}
                        </div>
                    </button>
                ))}
            </aside>
            <section
                className="execlaw-research__detail"
                data-testid="research-detail"
            >
                {selected === null ? (
                    <div className="m-3 execlaw-muted small">
                        Select a job on the left.
                    </div>
                ) : (
                    <ResearchJobDetail job={selected} />
                )}
            </section>
        </div>
    );
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

    return (
        <div data-testid="research-detail-body">
            <h3 className="h6 mb-2">{job.query}</h3>
            <div className="execlaw-muted small mb-3">
                <span className={`badge bg-${badgeColor(job.status)} me-2`}>
                    {job.status}
                </span>
                Job <code>{job.id}</code> · conversation{" "}
                <code>{job.conversation_id}</code>
            </div>
            {(canAdvance || canCancel) && (
                <div
                    className="d-flex gap-2 mb-3"
                    data-testid="research-detail-actions"
                >
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
            )}
            {actionError && (
                <div
                    className="execlaw-card-task__error mb-3"
                    data-testid="research-detail-action-error"
                >
                    {actionError}
                </div>
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
                <section className="mb-4" data-testid="research-detail-plan">
                    <h4 className="h6 execlaw-muted small">Plan</h4>
                    <p className="mb-2">
                        <span className="execlaw-muted small me-2">Thesis:</span>
                        {job.plan.thesis}
                    </p>
                    <ol>
                        {job.plan.steps.map((s, i) => {
                            const note = job.notes.find((n) => n.index === i);
                            const state = note?.state ?? "Pending";
                            return (
                                <li
                                    key={i}
                                    className="mb-2"
                                    data-testid="research-detail-step"
                                    data-state={state}
                                >
                                    <span
                                        className={`badge bg-${noteColor(state)} me-2`}
                                    >
                                        {state}
                                    </span>
                                    {s.query}
                                    {s.rationale && (
                                        <div className="execlaw-muted small">
                                            {s.rationale}
                                        </div>
                                    )}
                                    {note?.excerpt && (
                                        <div className="mt-1 small">
                                            {note.excerpt}
                                        </div>
                                    )}
                                    {note?.sources && note.sources.length > 0 && (
                                        <ul className="execlaw-muted small mt-1">
                                            {note.sources.map((src, j) => (
                                                <li key={`${src.url}-${j}`}>
                                                    {src.fetched_ok === false ? (
                                                        <span>
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
                    className="mb-3"
                    data-testid="research-detail-workspace"
                >
                    <h4 className="h6 execlaw-muted small">Workspace</h4>
                    <code className="execlaw-muted small">
                        {job.workspace_path}
                    </code>
                </section>
            )}
            {job.status === "complete" && (
                <section
                    className="mb-3"
                    data-testid="research-detail-report"
                >
                    <h4 className="h6 execlaw-muted small">Report</h4>
                    {!reportLoaded ? (
                        <div className="execlaw-muted small">Loading…</div>
                    ) : report === null ? (
                        <div className="execlaw-muted small">
                            No report on disk for this job (workspace may
                            have been retention-purged).
                        </div>
                    ) : (
                        <pre
                            className="execlaw-card-research__report-body"
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
