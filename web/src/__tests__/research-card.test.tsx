// Tests for the C4 ResearchCard renderer + cardStore projection.
//
// Two surfaces under test:
//
//   * `ResearchCard` — per-kind renderer for `kind: "research"`
//     cards. Reads `card.details.{plan, notes}` to paint the plan
//     tree with per-sub-query state badges.
//
//   * `cardStore` — projects WS card.* events into a per-conversation
//     `Map<card_id, Card>`. The `useCardsForConversation` hook
//     surfaces them sorted by `opened_at` so MessageStream can
//     interleave them with messages chronologically.

import { describe, expect, it, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { renderHook, act } from "@testing-library/react";
import { ResearchCard } from "../cards/ResearchCard";
import { getCardRenderer } from "../cards/CardRenderer";
import {
    __resetCardStore,
    applyCardEvent,
    setCardsForConversation,
    useCardsForConversation,
} from "../cards/cardStore";
import type { Card, CardEvent } from "../cards/types";

beforeEach(() => {
    __resetCardStore();
});

function makeResearchCard(extras: Partial<Card> = {}): Card {
    return {
        card_id: "card-1",
        conversation_id: "conv-1",
        kind: "research",
        state: "Running",
        title: "Research: Kokoro 2026 changes",
        summary: "Gathering · 1/3 done",
        progress: 0.5,
        phase: "Gathering",
        details: {
            job_id: "job-1",
            phase: "Gathering",
            plan: {
                thesis: "compare Kokoro 2026 vs Whisper-large-v3",
                steps: [
                    { query: "kokoro release notes 2026", rationale: "baseline" },
                    { query: "whisper benchmarks", rationale: null },
                    { query: "operator reports", rationale: null },
                ],
            },
            notes: [
                {
                    index: 0,
                    sub_query: "kokoro release notes 2026",
                    state: "Done",
                    excerpt: "Kokoro 2026 added 3 new voices.",
                    sources: [
                        {
                            url: "https://example.com/kokoro",
                            title: "Kokoro Release Notes",
                            fetched_ok: true,
                        },
                    ],
                    tokens_used: 200,
                },
                {
                    index: 1,
                    sub_query: "whisper benchmarks",
                    state: "Running",
                    excerpt: "",
                    sources: [],
                },
            ],
        },
        actions: [],
        attachment_id: null,
        error: null,
        opened_at: 100,
        updated_at: 150,
        event_seq: null,
        ...extras,
    };
}

describe("ResearchCard renderer", () => {
    it("registers itself for kind:research and is returned by getCardRenderer", () => {
        const Renderer = getCardRenderer("research");
        expect(Renderer).toBe(ResearchCard);
    });

    it("renders title, phase, progress bar, and thesis", () => {
        render(<ResearchCard card={makeResearchCard()} />);
        expect(screen.getByTestId("card-research")).toBeTruthy();
        expect(screen.getByText(/Research: Kokoro 2026 changes/)).toBeTruthy();
        expect(screen.getByTestId("card-phase").textContent).toContain(
            "Gathering",
        );
        expect(screen.getByTestId("card-progress")).toBeTruthy();
        expect(screen.getByTestId("card-research-thesis").textContent).toContain(
            "compare Kokoro",
        );
    });

    it("renders one PlanStepRow per plan.step with state badges", () => {
        render(<ResearchCard card={makeResearchCard()} />);
        const rows = screen.getAllByTestId("card-research-step");
        expect(rows).toHaveLength(3);
        const badges = screen.getAllByTestId("card-research-step-state");
        // First sub-query: Done. Second: Running. Third: Pending
        // (no note yet — falls through to seeded Pending).
        expect(badges[0].getAttribute("data-state")).toBe("Done");
        expect(badges[1].getAttribute("data-state")).toBe("Running");
        expect(badges[2].getAttribute("data-state")).toBe("Pending");
    });

    it("only shows the Show/Hide toggle when a note has detail", () => {
        // The third step has no note → no toggle. The first step
        // has excerpt + sources → toggle present.
        render(<ResearchCard card={makeResearchCard()} />);
        const toggles = screen.getAllByTestId("card-research-step-toggle");
        // Two notes have detail (Done has excerpt+sources, Running
        // has neither). So only one toggle.
        expect(toggles).toHaveLength(1);
    });

    it("expands the step detail when the operator clicks Show", () => {
        render(<ResearchCard card={makeResearchCard()} />);
        const toggle = screen.getByTestId("card-research-step-toggle");
        expect(screen.queryByTestId("card-research-step-detail")).toBeNull();
        fireEvent.click(toggle);
        const detail = screen.getByTestId("card-research-step-detail");
        expect(detail.textContent).toContain("Kokoro 2026 added 3 new voices.");
        expect(detail.querySelector("a")?.getAttribute("href")).toBe(
            "https://example.com/kokoro",
        );
    });

    it("renders a Failed source with strike-through and an error message", () => {
        const card = makeResearchCard({
            details: {
                job_id: "job-1",
                phase: "Gathering",
                plan: {
                    thesis: "x",
                    steps: [{ query: "q", rationale: null }],
                },
                notes: [
                    {
                        index: 0,
                        sub_query: "q",
                        state: "Done",
                        excerpt: "ok",
                        sources: [
                            {
                                url: "https://broken.example.com",
                                title: "Broken",
                                fetched_ok: false,
                                error: "404",
                            },
                        ],
                    },
                ],
            },
        });
        render(<ResearchCard card={card} />);
        fireEvent.click(screen.getByTestId("card-research-step-toggle"));
        const detail = screen.getByTestId("card-research-step-detail");
        expect(detail.textContent).toContain("✗");
        expect(detail.textContent).toContain("404");
    });

    it("falls back gracefully when details is malformed", () => {
        const card = makeResearchCard();
        // Wipe details — renderer must still emit a card without
        // crashing.
        card.details = "not an object";
        render(<ResearchCard card={card} />);
        expect(screen.getByTestId("card-research")).toBeTruthy();
    });

    it("does NOT render the report markdown inline (delivery is via send_attachment)", () => {
        // 2026-05-03 (rev 3): the inline `<pre>{report_markdown}</pre>`
        // block was removed. Reports were 1000+ lines and bloated the
        // chat pane. The PDF flows through `send_attachment` →
        // AttachmentCard chip instead. Even when stale data carries
        // a `report_markdown` field, the ResearchCard must NOT
        // render it.
        const card = makeResearchCard({
            state: "Completed",
            progress: 1,
            details: {
                job_id: "job-1",
                phase: "Complete",
                plan: {
                    thesis: "test",
                    steps: [{ query: "q", rationale: null }],
                },
                notes: [
                    {
                        index: 0,
                        sub_query: "q",
                        state: "Done",
                        excerpt: "ok",
                        sources: [],
                    },
                ],
                report_markdown: "# Final report\n\nKey findings.",
                report_url: "/research/job-1",
            } as Record<string, unknown>,
        });
        render(<ResearchCard card={card} />);
        expect(screen.queryByTestId("card-research-report")).toBeNull();
        expect(screen.queryByTestId("card-research-report-body")).toBeNull();
    });

    it("never includes a progress bar when state is Completed", () => {
        const card = makeResearchCard({ state: "Completed", progress: 1 });
        render(<ResearchCard card={card} />);
        expect(screen.queryByTestId("card-progress")).toBeNull();
    });

    it("renders the awaiting-input panel with the planner's clarification question", () => {
        // 2026-05-03 (rev 2): the runner pauses the job in
        // AwaitingInput when the planner judges the query too vague.
        // The card surfaces the question so the operator sees that
        // research is paused; the agent (chat's primary interface)
        // is what actually asks the user the question.
        const card = makeResearchCard({
            phase: "AwaitingInput",
            state: "Running",
            progress: 0.33,
            details: {
                job_id: "job-1",
                phase: "AwaitingInput",
                clarification_question:
                    "Which USDA hardiness zone are you planting in?",
                query: "Recommend evergreen ground covers.",
            },
        });
        render(<ResearchCard card={card} />);
        const panel = screen.getByTestId("card-research-clarification");
        expect(panel).toBeTruthy();
        const q = screen.getByTestId("card-research-clarification-question");
        expect(q.textContent).toContain("USDA hardiness zone");
        // The "agent will ask you" hint must be present so the
        // operator knows where to provide the answer.
        expect(panel.textContent).toMatch(/agent will ask you/i);
    });

    it("does not render the awaiting-input panel for other phases", () => {
        // Defensive: only show clarification when the card actually
        // is in the AwaitingInput phase. A stale clarification field
        // bleeding into a Gathering card would mislead the operator.
        const card = makeResearchCard({
            phase: "Gathering",
            details: {
                job_id: "job-1",
                phase: "Gathering",
                clarification_question: "stale leftover",
            },
        });
        render(<ResearchCard card={card} />);
        expect(screen.queryByTestId("card-research-clarification")).toBeNull();
    });
});

// ---- cardStore -------------------------------------------------------

function openedEvent(card_id: string, _conv: string, ts: number): CardEvent {
    return {
        kind: "card.opened",
        committed_at: ts,
        payload: {
            card_id,
            kind: "research",
            title: `Title ${card_id}`,
            summary: `Summary ${card_id}`,
        },
    };
}

function progressedEvent(
    card_id: string,
    progress: number,
    ts: number,
): CardEvent {
    return {
        kind: "card.progressed",
        committed_at: ts,
        payload: { card_id, progress },
    };
}

describe("cardStore + useCardsForConversation", () => {
    it("returns an empty array when no cards exist for the conversation", () => {
        const { result } = renderHook(() => useCardsForConversation("conv-x"));
        expect(result.current).toEqual([]);
    });

    it("projects an Opened event into the store and re-renders consumers", () => {
        const { result } = renderHook(() =>
            useCardsForConversation("conv-1"),
        );
        expect(result.current).toHaveLength(0);
        act(() => {
            applyCardEvent("conv-1", openedEvent("c-a", "conv-1", 100));
        });
        expect(result.current).toHaveLength(1);
        expect(result.current[0].card_id).toBe("c-a");
    });

    it("scopes cards per conversation (no cross-conv bleed)", () => {
        const { result: convA } = renderHook(() =>
            useCardsForConversation("conv-A"),
        );
        const { result: convB } = renderHook(() =>
            useCardsForConversation("conv-B"),
        );
        act(() => {
            applyCardEvent("conv-A", openedEvent("a", "conv-A", 100));
            applyCardEvent("conv-B", openedEvent("b", "conv-B", 110));
        });
        expect(convA.current).toHaveLength(1);
        expect(convA.current[0].card_id).toBe("a");
        expect(convB.current).toHaveLength(1);
        expect(convB.current[0].card_id).toBe("b");
    });

    it("merges Progressed onto an open card; out-of-band Progressed is a no-op", () => {
        const { result } = renderHook(() =>
            useCardsForConversation("conv-1"),
        );
        act(() => {
            applyCardEvent("conv-1", openedEvent("a", "conv-1", 100));
            applyCardEvent("conv-1", progressedEvent("a", 0.5, 150));
        });
        expect(result.current[0].progress).toBe(0.5);
        // Now an event for a card we never saw — must NOT spawn a
        // ghost card in the projection.
        act(() => {
            applyCardEvent("conv-1", progressedEvent("ghost", 0.7, 200));
        });
        expect(result.current).toHaveLength(1);
    });

    it("sorts cards by opened_at ascending", () => {
        const { result } = renderHook(() =>
            useCardsForConversation("conv-1"),
        );
        act(() => {
            applyCardEvent("conv-1", openedEvent("late", "conv-1", 200));
            applyCardEvent("conv-1", openedEvent("early", "conv-1", 100));
        });
        expect(result.current.map((c) => c.card_id)).toEqual(["early", "late"]);
    });

    /// 2026-05-04 regression: cards used to be live-WS-only state;
    /// a page refresh dropped every inline card even though the
    /// underlying CardOpened/Closed events were durably persisted.
    /// Chat.tsx now fetches /api/chats/{cid}/cards on thread load
    /// and seeds the store via setCardsForConversation. Asserts:
    ///   * setCardsForConversation overwrites the conversation's
    ///     map in one shot
    ///   * subsequent applyCardEvent merges per-card on top
    ///     (live events arriving after hydration still work)
    it("setCardsForConversation hydrates the store; live events merge on top", () => {
        const { result } = renderHook(() =>
            useCardsForConversation("conv-hydrate"),
        );
        expect(result.current).toHaveLength(0);

        // Simulated `GET /api/chats/{cid}/cards` response — two
        // already-completed attachment cards.
        act(() => {
            setCardsForConversation("conv-hydrate", [
                {
                    card_id: "att-1",
                    conversation_id: "conv-hydrate",
                    kind: "attachment",
                    state: "Completed",
                    title: "report-a.pdf",
                    summary: "report-a.pdf",
                    progress: null,
                    phase: null,
                    details: { attachment_id: "att-1" },
                    actions: [],
                    error: null,
                    opened_at: 1,
                    updated_at: 1,
                    attachment_id: "att-1",
                    event_seq: 5,
                },
                {
                    card_id: "att-2",
                    conversation_id: "conv-hydrate",
                    kind: "attachment",
                    state: "Completed",
                    title: "report-b.pdf",
                    summary: "report-b.pdf",
                    progress: null,
                    phase: null,
                    details: { attachment_id: "att-2" },
                    actions: [],
                    error: null,
                    opened_at: 2,
                    updated_at: 2,
                    attachment_id: "att-2",
                    event_seq: 7,
                },
            ]);
        });
        expect(result.current.map((c) => c.card_id)).toEqual(["att-1", "att-2"]);

        // Live event after hydration: a new card opens. Merges
        // into the same conversation map (doesn't clobber the
        // hydrated set).
        act(() => {
            applyCardEvent(
                "conv-hydrate",
                openedEvent("att-3", "conv-hydrate", 100),
            );
        });
        expect(result.current.map((c) => c.card_id)).toEqual([
            "att-1",
            "att-2",
            "att-3",
        ]);
    });

    it("setCardsForConversation called twice replaces prior contents (no merge)", () => {
        // The endpoint returns the canonical state — a second
        // hydration call (e.g. operator switches threads + back)
        // should overwrite, not append.
        const { result } = renderHook(() =>
            useCardsForConversation("conv-replace"),
        );
        act(() => {
            setCardsForConversation("conv-replace", [
                {
                    card_id: "old",
                    conversation_id: "conv-replace",
                    kind: "research",
                    state: "Completed",
                    title: "Old",
                    summary: "Old",
                    progress: null,
                    phase: null,
                    details: {},
                    actions: [],
                    error: null,
                    opened_at: 1,
                    updated_at: 1,
                    attachment_id: null,
                    event_seq: 1,
                },
            ]);
        });
        expect(result.current.map((c) => c.card_id)).toEqual(["old"]);
        act(() => {
            setCardsForConversation("conv-replace", [
                {
                    card_id: "new",
                    conversation_id: "conv-replace",
                    kind: "research",
                    state: "Completed",
                    title: "New",
                    summary: "New",
                    progress: null,
                    phase: null,
                    details: {},
                    actions: [],
                    error: null,
                    opened_at: 2,
                    updated_at: 2,
                    attachment_id: null,
                    event_seq: 2,
                },
            ]);
        });
        expect(result.current.map((c) => c.card_id)).toEqual(["new"]);
    });
});
