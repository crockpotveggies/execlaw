// Per-kind renderer for `kind: research` cards (C4).
//
// Replaces the generic `LongRunningTaskCard` for research jobs. The
// renderer reads the plan tree + per-sub-query notes from the card's
// `details` object, which the runner shapes as:
//
//   {
//     job_id: string,
//     phase: "Planning" | "Planned" | "Gathering" | "Gather complete",
//     query?: string,                  // original research question
//     plan?: { thesis: string, steps: { query, rationale? }[] },
//     notes?: ResearchNote[],          // per-sub-query state
//   }
//
// Falls back to the generic LongRunningTask renderer when `details`
// is missing or malformed (defensive — runner should always supply
// it, but a future schema bump should not break the chat pane).
//
// 2026-04-29.

import { useContext, useState } from "react";
import { AuthContext } from "../auth/AuthContext";
import { registerCardRenderer, type CardRendererProps } from "./CardRenderer";
import type { Card } from "./types";

type SubQueryState = "Pending" | "Running" | "Done" | "Failed";

interface ResearchSourceView {
    url: string;
    title?: string | null;
    fetched_ok?: boolean;
    error?: string | null;
}

interface ResearchNoteView {
    index: number;
    sub_query: string;
    state: SubQueryState;
    excerpt: string;
    sources: ResearchSourceView[];
    tokens_used?: number | null;
    error?: string | null;
}

interface ResearchPlanView {
    thesis: string;
    steps: { query: string; rationale?: string | null }[];
}

interface ResearchDetails {
    job_id?: string;
    phase?: string;
    query?: string;
    plan?: ResearchPlanView;
    notes?: ResearchNoteView[];
    /// Set on `CardProgressed { phase: "AwaitingInput" }` when the
    /// planner judged the query too vague and posed a clarification
    /// question. The agent (chat's primary interface per
    /// project-locked-decisions 2026-04-23) relays this to the user
    /// in conversation; the card surfaces it as visual confirmation
    /// that research is paused waiting on the agent + user exchange.
    clarification_question?: string;
    /// Reserved for the /research/<job_id> deep-link. The card itself
    /// no longer renders the report inline; the agent delivers the
    /// PDF via `send_attachment` (see AttachmentCard) and that chip
    /// lands as a sibling card immediately after this one.
    report_url?: string;
}

function readDetails(raw: unknown): ResearchDetails | null {
    if (!raw || typeof raw !== "object") return null;
    return raw as ResearchDetails;
}

export function ResearchCard({ card, onAction }: CardRendererProps) {
    const details = readDetails(card.details);
    const pct =
        card.progress !== null ? Math.round(card.progress * 100) : null;
    const showProgress = pct !== null && card.state !== "Completed";
    const plan = details?.plan ?? null;
    const notes = details?.notes ?? [];
    // Lookup table: index → note. Plans whose corresponding note
    // hasn't landed yet stay Pending.
    const noteByIndex = new Map<number, ResearchNoteView>();
    for (const n of notes) noteByIndex.set(n.index, n);
    const doneCount = notes.filter((n) => n.state === "Done").length;
    const failedCount = notes.filter((n) => n.state === "Failed").length;
    const totalSteps = plan?.steps.length ?? notes.length;

    return (
        <div
            className="execlaw-card-task execlaw-card-research"
            data-testid="card-research"
            data-card-id={card.card_id}
            data-state={card.state}
        >
            <div className="execlaw-card-task__head">
                <span className="execlaw-card-task__title">
                    <i
                        className="bi bi-binoculars me-2"
                        aria-hidden
                    />
                    {card.title}
                </span>
                <span
                    className="execlaw-card-task__state"
                    data-testid="card-state"
                >
                    {card.state}
                </span>
            </div>

            {card.phase && (
                <div
                    className="execlaw-card-task__phase"
                    data-testid="card-phase"
                >
                    {card.phase}
                    {totalSteps > 0 && doneCount + failedCount > 0 && (
                        <span className="ms-2 execlaw-muted">
                            · {doneCount}/{totalSteps} done
                            {failedCount > 0 && ` · ${failedCount} failed`}
                        </span>
                    )}
                </div>
            )}

            {showProgress && (
                <div
                    className="execlaw-card-task__progress"
                    role="progressbar"
                    aria-valuenow={pct ?? 0}
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-label="Research job progress"
                    data-testid="card-progress"
                >
                    <div
                        className="execlaw-card-task__progress-bar"
                        style={{ width: `${pct}%` }}
                    />
                    <span className="execlaw-card-task__progress-label">
                        {pct}%
                    </span>
                </div>
            )}

            {plan?.thesis && (
                <div
                    className="execlaw-card-research__thesis"
                    data-testid="card-research-thesis"
                >
                    <span className="execlaw-muted small me-2">Thesis:</span>
                    {plan.thesis}
                </div>
            )}

            {plan && plan.steps.length > 0 && (
                <ol
                    className="execlaw-card-research__plan"
                    data-testid="card-research-plan"
                >
                    {plan.steps.map((step, i) => {
                        const note = noteByIndex.get(i);
                        const state: SubQueryState = note?.state ?? "Pending";
                        return (
                            <PlanStepRow
                                key={i}
                                index={i}
                                query={step.query}
                                rationale={step.rationale ?? null}
                                state={state}
                                note={note ?? null}
                            />
                        );
                    })}
                </ol>
            )}

            {details?.clarification_question && card.phase === "AwaitingInput" && (
                <div
                    className="execlaw-card-research__clarification"
                    data-testid="card-research-clarification"
                    role="status"
                >
                    <div className="execlaw-card-research__clarification-label">
                        <i
                            className="bi bi-chat-left-quote me-2"
                            aria-hidden
                        />
                        Awaiting clarification
                    </div>
                    <div
                        className="execlaw-card-research__clarification-question"
                        data-testid="card-research-clarification-question"
                    >
                        {details.clarification_question}
                    </div>
                    <div className="execlaw-card-research__clarification-hint execlaw-muted small">
                        The agent will ask you about this in chat — answer there
                        and the research will resume automatically.
                    </div>
                </div>
            )}

            <div className="execlaw-card-task__summary">{card.summary}</div>

            {/*
              * 2026-05-04 (rev 4): the PDF deliverable now lives on
              * THIS card as a Download button (rendered below) rather
              * than via a separate Attachment chip. Two reasons:
              *   1. The separate chip + parent research card created
              *      visual duplication ("research complete" message
              *      followed by "here's a file" chip) that read as
              *      sloppy / two-step instead of one cohesive
              *      result.
              *   2. The chip's render diverged between live
              *      (CardOpened/Closed via send_attachment) and
              *      replayed (cardStore hydration on refresh)
              *      paths, making refresh feel buggy.
              * The download button just inlines into this card; both
              * live and replayed render through the same code path
              * because attachment_id is part of the card row itself.
              */}
            <DownloadButton card={card} />


            {card.error && (
                <div
                    className="execlaw-card-task__error"
                    data-testid="card-error"
                >
                    {card.error}
                </div>
            )}

            {card.actions.length > 0 && onAction && (
                <div className="execlaw-card-task__actions">
                    {card.actions.map((a, idx) => {
                        const id =
                            a.kind === "Cancel"
                                ? "cancel"
                                : a.kind === "Pause"
                                  ? "pause"
                                  : a.kind === "Resume"
                                    ? "resume"
                                    : "open_detail";
                        return (
                            <button
                                key={`${id}-${idx}`}
                                type="button"
                                className="execlaw-card-task__action"
                                onClick={() => onAction(id)}
                                data-testid={`card-action-${id}`}
                            >
                                {a.kind === "OpenDetail"
                                    ? "Open"
                                    : a.kind}
                            </button>
                        );
                    })}
                </div>
            )}
        </div>
    );
}

function PlanStepRow({
    index,
    query,
    rationale,
    state,
    note,
}: {
    index: number;
    query: string;
    rationale: string | null;
    state: SubQueryState;
    note: ResearchNoteView | null;
}) {
    const [expanded, setExpanded] = useState(false);
    const hasDetail =
        note !== null &&
        ((note.excerpt && note.excerpt.length > 0) ||
            note.sources.length > 0 ||
            note.error);
    return (
        <li
            className="execlaw-card-research__step"
            data-testid="card-research-step"
            data-step-index={index}
            data-state={state}
        >
            <div className="execlaw-card-research__step-head">
                <StatusBadge state={state} />
                <span className="execlaw-card-research__step-query">
                    {query}
                </span>
                {hasDetail && (
                    <button
                        type="button"
                        className="btn btn-link btn-sm p-0 ms-2 execlaw-muted"
                        onClick={() => setExpanded((v) => !v)}
                        aria-expanded={expanded}
                        data-testid="card-research-step-toggle"
                    >
                        {expanded ? "Hide" : "Show"}
                    </button>
                )}
            </div>
            {rationale && (
                <div className="execlaw-card-research__step-rationale execlaw-muted small">
                    {rationale}
                </div>
            )}
            {expanded && note && (
                <div
                    className="execlaw-card-research__step-detail"
                    data-testid="card-research-step-detail"
                >
                    {note.error && (
                        <div className="execlaw-card-task__error">
                            {note.error}
                        </div>
                    )}
                    {note.excerpt && (
                        <div className="execlaw-card-research__step-excerpt">
                            {note.excerpt}
                        </div>
                    )}
                    {note.sources.length > 0 && (
                        <ul className="execlaw-card-research__sources">
                            {note.sources.map((s, i) => (
                                <li key={`${s.url}-${i}`}>
                                    {s.fetched_ok === false ? (
                                        <span className="execlaw-muted">
                                            ✗ {s.url}
                                            {s.error && ` — ${s.error}`}
                                        </span>
                                    ) : (
                                        <a
                                            href={s.url}
                                            target="_blank"
                                            rel="noopener noreferrer"
                                        >
                                            {s.title ?? s.url}
                                        </a>
                                    )}
                                </li>
                            ))}
                        </ul>
                    )}
                </div>
            )}
        </li>
    );
}

function StatusBadge({ state }: { state: SubQueryState }) {
    const cls = {
        Pending: "secondary",
        Running: "primary",
        Done: "success",
        Failed: "danger",
    }[state];
    const label = state;
    return (
        <span
            className={`badge bg-${cls} me-2`}
            data-testid="card-research-step-state"
            data-state={state}
        >
            {label}
        </span>
    );
}

/// Inline download button for the research deliverable. Renders
/// only when:
///   * The card has an `attachment_id` set (synthesise produced a
///     PDF and the runner stamped it on CardClosed)
///   * The card is in a terminal Completed state (the runner sets
///     attachment_id BEFORE closing, so this gate ensures we don't
///     show a stale link mid-run)
///
/// Replaces the previous "separate Attachment card chip" rendering.
/// Same persistence guarantees as any other ResearchCard field —
/// attachment_id flows through CardClosed payload and survives
/// page refresh via the listCards projection.
function DownloadButton({ card }: { card: Card }) {
    const auth = useContext(AuthContext);
    if (card.state !== "Completed") return null;
    if (!card.attachment_id) return null;
    const token = auth?.getAccessToken() ?? null;
    const baseUrl = `/api/attachments/${card.attachment_id}`;
    // Token in query-string so the `<a download>` link
    // authenticates — browsers don't carry the Authorization
    // header on link navigations. See auth_extract.rs's
    // `?access_token=…` fallback.
    const url = token
        ? `${baseUrl}?access_token=${encodeURIComponent(token)}`
        : baseUrl;
    return (
        <div
            className="execlaw-card-research__download"
            data-testid="card-research-download"
        >
            <a
                className="btn btn-sm btn-primary"
                href={url}
                download
                data-testid="card-research-download-link"
            >
                <i className="bi bi-download me-1" aria-hidden />
                Download report (PDF)
            </a>
        </div>
    );
}

// Auto-register on module load. The chat-pane import-side-effect-
// loads this file so the registry has a ResearchCard ready when the
// first `kind: research` card lands.
registerCardRenderer("research", ResearchCard);
