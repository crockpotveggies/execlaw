// Tests for the generic card primitive (PR C1b — 2026-04-29).
//
// Two surfaces under test:
//
//   * `applyEvent` / `fromOpened` / `projectFromEvents` in
//     `web/src/cards/projection.ts` — TypeScript mirror of the
//     Rust `Card::apply` projection. Behavior must match the Rust
//     side test-for-test so a server-side change has a check on
//     this side too.
//
//   * `LongRunningTaskCard` renderer + the registry resolver. The
//     registry's fallback-to-LongRunningTask behavior is what
//     keeps the SPA forwards-compatible against plugin-emitted
//     unknown kinds.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import {
    applyEvent,
    fromOpened,
    projectFromEvents,
} from "../cards/projection";
import {
    getCardRenderer,
    LongRunningTaskCard,
    registerCardRenderer,
} from "../cards/CardRenderer";
import type { Card, CardEvent, CardKind } from "../cards/types";

// ---- Projection ------------------------------------------------------

function opened(
    card_id: string,
    overrides: Partial<{ kind: CardKind; ts: number }> = {},
): CardEvent {
    return {
        kind: "card.opened",
        committed_at: overrides.ts ?? 100,
        payload: {
            card_id,
            kind: overrides.kind ?? "long_running_task",
            title: `Title ${card_id}`,
            summary: `Summary ${card_id}`,
        },
    };
}

function progressed(
    card_id: string,
    extras: Partial<{
        progress: number;
        phase: string;
        summary: string;
        state: Card["state"];
        ts: number;
    }> = {},
): CardEvent {
    return {
        kind: "card.progressed",
        committed_at: extras.ts ?? 200,
        payload: {
            card_id,
            progress: extras.progress,
            phase: extras.phase,
            summary: extras.summary,
            state: extras.state,
        },
    };
}

function closed(
    card_id: string,
    state: Card["state"] = "Completed",
    extras: Partial<{ summary: string; attachment_id: string; error: string; ts: number }> = {},
): CardEvent {
    return {
        kind: "card.closed",
        committed_at: extras.ts ?? 500,
        payload: {
            card_id,
            state,
            summary: extras.summary ?? "done",
            attachment_id: extras.attachment_id,
            error: extras.error,
        },
    };
}

describe("card projection", () => {
    it("fromOpened seeds Pending state by default", () => {
        const card = fromOpened("c1", opened("a"));
        expect(card).not.toBeNull();
        expect(card!.card_id).toBe("a");
        expect(card!.state).toBe("Pending");
        expect(card!.opened_at).toBe(100);
        expect(card!.updated_at).toBe(100);
    });

    it("applyEvent merges progress + phase + summary on Progressed", () => {
        const c0 = fromOpened("c1", opened("a"))!;
        const c1 = applyEvent(
            c0,
            progressed("a", {
                progress: 0.5,
                phase: "Gathering",
                summary: "halfway",
                state: "Running",
                ts: 150,
            }),
        );
        expect(c1.state).toBe("Running");
        expect(c1.progress).toBe(0.5);
        expect(c1.phase).toBe("Gathering");
        expect(c1.summary).toBe("halfway");
        expect(c1.updated_at).toBe(150);
        // Original card untouched (pure-functional contract).
        expect(c0.progress).toBeNull();
    });

    it("applyEvent clamps progress to [0, 1]", () => {
        const c0 = fromOpened("c1", opened("a"))!;
        const overshoot = applyEvent(c0, progressed("a", { progress: 1.5 }));
        expect(overshoot.progress).toBe(1);
        const undershoot = applyEvent(c0, progressed("a", { progress: -0.3 }));
        expect(undershoot.progress).toBe(0);
    });

    it("applyEvent ignores events for a different card_id", () => {
        const c0 = fromOpened("c1", opened("a"))!;
        const ev = progressed("DIFFERENT", { progress: 0.9 });
        const c1 = applyEvent(c0, ev);
        // Same reference returned — telegraph "no change" so React's
        // reference equality keeps the component stable.
        expect(c1).toBe(c0);
    });

    it("applyEvent on Closed flips state and summary, carries attachment", () => {
        const c0 = fromOpened("c1", opened("a"))!;
        const c1 = applyEvent(c0, closed("a", "Completed", { summary: "report ready", attachment_id: "att-1", ts: 400 }));
        expect(c1.state).toBe("Completed");
        expect(c1.summary).toBe("report ready");
        expect(c1.attachment_id).toBe("att-1");
        expect(c1.updated_at).toBe(400);
    });

    it("applyEvent on Failed carries error", () => {
        const c0 = fromOpened("c1", opened("a"))!;
        const c1 = applyEvent(c0, closed("a", "Failed", { error: "OOM" }));
        expect(c1.state).toBe("Failed");
        expect(c1.error).toBe("OOM");
    });

    it("projectFromEvents replays a full sequence into a final card", () => {
        const events: CardEvent[] = [
            opened("a"),
            progressed("a", { progress: 0.1, phase: "Plan", ts: 110 }),
            progressed("a", { progress: 0.4, phase: "Gather", ts: 200 }),
            progressed("a", { progress: 0.9, phase: "Synth", ts: 300 }),
            closed("a", "Completed", { ts: 400 }),
        ];
        const cards = projectFromEvents("c1", events);
        expect(cards.size).toBe(1);
        const card = cards.get("a")!;
        expect(card.state).toBe("Completed");
        expect(card.progress).toBe(0.9);
        // 2026-05-04: phase no longer survives a Closed event.
        // Closed apply re-derives phase from details.phase (None
        // here because the closed() helper omits details), so the
        // last Progressed phase ("Synth") doesn't leak past the
        // close. This was the bug surfaced by users seeing
        // "Synthesizing" rendered below a "Completed" badge.
        expect(card.phase).toBe(null);
        expect(card.updated_at).toBe(400);
    });

    it("projectFromEvents skips out-of-band Progressed without an Open", () => {
        const events: CardEvent[] = [
            // Closed for a card that never got Opened. Skipped.
            closed("ghost", "Completed"),
            opened("real"),
            progressed("real", { progress: 0.5 }),
        ];
        const cards = projectFromEvents("c1", events);
        expect(cards.has("ghost")).toBe(false);
        expect(cards.has("real")).toBe(true);
        expect(cards.get("real")!.progress).toBe(0.5);
    });

    it("projectFromEvents handles multiple concurrent cards", () => {
        const events: CardEvent[] = [
            opened("a"),
            opened("b"),
            progressed("a", { progress: 0.3 }),
            progressed("b", { progress: 0.7 }),
            closed("a", "Cancelled"),
        ];
        const cards = projectFromEvents("c1", events);
        expect(cards.size).toBe(2);
        expect(cards.get("a")!.state).toBe("Cancelled");
        expect(cards.get("a")!.progress).toBe(0.3);
        expect(cards.get("b")!.state).toBe("Pending");
        expect(cards.get("b")!.progress).toBe(0.7);
    });
});

// ---- Renderer registry -----------------------------------------------

describe("card renderer registry", () => {
    it("returns LongRunningTask for the explicit kind", () => {
        const r = getCardRenderer("long_running_task");
        expect(r).toBe(LongRunningTaskCard);
    });

    it("falls back to LongRunningTask for an unknown kind", () => {
        // Cast to bypass the strict union — we want to simulate a
        // future plugin emitting a kind we don't ship a renderer for.
        const r = getCardRenderer("bogus_kind" as unknown as CardKind);
        expect(r).toBe(LongRunningTaskCard);
    });

    it("uses a custom renderer once registered", () => {
        const Stub = () => <div data-testid="stub" />;
        registerCardRenderer("file_pipeline", Stub);
        expect(getCardRenderer("file_pipeline")).toBe(Stub);
    });
});

// ---- LongRunningTaskCard component -----------------------------------

function makeCard(overrides: Partial<Card> = {}): Card {
    return {
        card_id: "c1",
        conversation_id: "conv-1",
        kind: "long_running_task",
        state: "Running",
        title: "Test task",
        summary: "doing the thing",
        progress: 0.4,
        phase: "Phase 1",
        details: {},
        actions: [],
        attachment_id: null,
        error: null,
        opened_at: 0,
        updated_at: 0,
        event_seq: null,
        ...overrides,
    };
}

describe("LongRunningTaskCard", () => {
    it("renders title, phase, summary, and progress", () => {
        render(<LongRunningTaskCard card={makeCard()} />);
        expect(screen.getByText("Test task")).toBeInTheDocument();
        expect(screen.getByTestId("card-phase")).toHaveTextContent("Phase 1");
        expect(screen.getByText("doing the thing")).toBeInTheDocument();
        const bar = screen.getByTestId("card-progress");
        expect(bar.getAttribute("aria-valuenow")).toBe("40");
    });

    it("hides the progress bar once the card is Completed", () => {
        render(
            <LongRunningTaskCard
                card={makeCard({ state: "Completed", progress: 1 })}
            />,
        );
        expect(screen.queryByTestId("card-progress")).not.toBeInTheDocument();
    });

    it("shows the error block when card.error is set", () => {
        render(
            <LongRunningTaskCard
                card={makeCard({ state: "Failed", error: "OOM at worker 3" })}
            />,
        );
        expect(screen.getByTestId("card-error")).toHaveTextContent(
            "OOM at worker 3",
        );
    });

    it("dispatches actions through onAction", () => {
        const onAction = vi.fn();
        render(
            <LongRunningTaskCard
                card={makeCard({
                    actions: [{ kind: "Cancel" }, { kind: "Pause" }],
                })}
                onAction={onAction}
            />,
        );
        fireEvent.click(screen.getByTestId("card-action-cancel"));
        fireEvent.click(screen.getByTestId("card-action-pause"));
        expect(onAction).toHaveBeenCalledTimes(2);
        expect(onAction).toHaveBeenCalledWith("cancel");
        expect(onAction).toHaveBeenCalledWith("pause");
    });

    it("does not render the action row when card has no actions", () => {
        render(<LongRunningTaskCard card={makeCard()} onAction={() => {}} />);
        expect(
            screen.queryByTestId("card-action-cancel"),
        ).not.toBeInTheDocument();
    });
});
