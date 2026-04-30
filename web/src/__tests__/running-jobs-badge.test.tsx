// Tests for the C6b RunningJobsBadge.
//
// Reads from the existing per-conversation card store; the badge
// is always live without any extra fetch. Cases under test:
//
//   * No active research cards → badge is absent (no DOM weight).
//   * One active research card → singular phrase + a deep-link to
//     the job's drill-down.
//   * Multiple active → plural phrase + the global /research link.
//   * Terminal cards (Completed/Failed/Cancelled) don't count.
//   * Non-research cards (LongRunningTask, etc.) don't count.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { RunningJobsBadge } from "../chat/RunningJobsBadge";
import {
    __resetCardStore,
    applyCardEvent,
} from "../cards/cardStore";
import type { CardEvent, CardKind } from "../cards/types";

beforeEach(() => {
    __resetCardStore();
});
afterEach(() => {
    __resetCardStore();
});

function openedEvent(
    card_id: string,
    kind: CardKind,
    ts: number,
    extras: Partial<{
        state: "Pending" | "Running" | "Completed" | "Failed" | "Cancelled";
        details: unknown;
    }> = {},
): CardEvent {
    return {
        kind: "card.opened",
        committed_at: ts,
        payload: {
            card_id,
            kind,
            title: `t-${card_id}`,
            summary: `s-${card_id}`,
            state: extras.state ?? "Running",
            details: extras.details ?? { job_id: `job-${card_id}` },
        },
    };
}

function renderBadge(conv: string) {
    return render(
        <MemoryRouter>
            <RunningJobsBadge conversationId={conv} />
        </MemoryRouter>,
    );
}

describe("RunningJobsBadge", () => {
    it("renders nothing when no active research cards exist", () => {
        renderBadge("conv-empty");
        expect(screen.queryByTestId("running-jobs-badge")).toBeNull();
    });

    it("renders a singular badge with a deep-link when one job is active", () => {
        // Mirror the runner's two-event sequence: Open with state +
        // details, then Progressed with the phase string.
        act(() => {
            applyCardEvent(
                "conv-1",
                openedEvent("c-a", "research", 100, {
                    state: "Running",
                    details: { job_id: "job-abc" },
                }),
            );
            applyCardEvent("conv-1", {
                kind: "card.progressed",
                committed_at: 110,
                payload: {
                    card_id: "c-a",
                    phase: "Gathering",
                    progress: 0.5,
                },
            });
        });
        renderBadge("conv-1");
        const badge = screen.getByTestId("running-jobs-badge");
        expect(badge.getAttribute("data-count")).toBe("1");
        const link = screen.getByTestId("running-jobs-badge-link");
        // Single-job badge deep-links to the specific job.
        expect(link.getAttribute("href")).toBe("/research/job-abc");
        // Phase line surfaces the runner's phase string.
        expect(badge.textContent).toContain("Gathering");
    });

    it("renders a plural badge linking to /research when multiple are active", () => {
        act(() => {
            applyCardEvent(
                "conv-multi",
                openedEvent("c-a", "research", 100),
            );
            applyCardEvent(
                "conv-multi",
                openedEvent("c-b", "research", 110),
            );
            applyCardEvent(
                "conv-multi",
                openedEvent("c-c", "research", 120),
            );
        });
        renderBadge("conv-multi");
        const badge = screen.getByTestId("running-jobs-badge");
        expect(badge.getAttribute("data-count")).toBe("3");
        expect(badge.textContent).toContain("3 research jobs running");
        const link = screen.getByTestId("running-jobs-badge-link");
        expect(link.getAttribute("href")).toBe("/research");
    });

    it("excludes terminal cards from the count", () => {
        act(() => {
            applyCardEvent(
                "conv-mixed",
                openedEvent("done", "research", 100, { state: "Completed" }),
            );
            applyCardEvent(
                "conv-mixed",
                openedEvent("failed", "research", 110, { state: "Failed" }),
            );
            applyCardEvent(
                "conv-mixed",
                openedEvent("cancelled", "research", 120, {
                    state: "Cancelled",
                }),
            );
        });
        renderBadge("conv-mixed");
        // Every card is terminal → badge absent.
        expect(screen.queryByTestId("running-jobs-badge")).toBeNull();
    });

    it("excludes non-research cards from the count", () => {
        // A LongRunningTask card on the same conversation must not
        // bump the research badge's count — the badge is exclusively
        // for research jobs (operators have other affordances for
        // non-research tasks).
        act(() => {
            applyCardEvent(
                "conv-other",
                openedEvent("shell", "long_running_task", 100),
            );
        });
        renderBadge("conv-other");
        expect(screen.queryByTestId("running-jobs-badge")).toBeNull();
    });

    it("falls back to card_id when details has no job_id", () => {
        act(() => {
            applyCardEvent(
                "conv-fallback",
                openedEvent("c-x", "research", 100, { details: {} }),
            );
        });
        renderBadge("conv-fallback");
        const link = screen.getByTestId("running-jobs-badge-link");
        expect(link.getAttribute("href")).toBe("/research/c-x");
    });
});
