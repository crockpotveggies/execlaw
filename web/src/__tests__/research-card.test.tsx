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

    it("never includes a progress bar when state is Completed", () => {
        const card = makeResearchCard({ state: "Completed", progress: 1 });
        render(<ResearchCard card={card} />);
        expect(screen.queryByTestId("card-progress")).toBeNull();
    });
});

// ---- cardStore -------------------------------------------------------

function openedEvent(card_id: string, conv: string, ts: number): CardEvent {
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
});
