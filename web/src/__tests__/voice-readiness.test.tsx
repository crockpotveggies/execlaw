// Regression test for the 2026-04-28 render-loop bug.
//
// `useVoiceReadiness(getToken)` used to recreate its `compute`
// callback whenever `getToken` changed, which fired
// `useEffect([compute])` on every render. Callers that passed a
// fresh arrow each render — e.g.
// `useVoiceReadiness(() => auth.getAccessToken())` from
// WelcomeView — turned this into a hot loop:
//
//   render → compute → setState → render → new arrow →
//   new compute → useEffect re-fires → another fetch → ...
//
// On localhost the fetch is sub-millisecond so the SPA pinned a
// CPU and flooded the control plane with hundreds of
// `/api/admin/backends` calls per second until the server
// crashed.
//
// The fix pins the getToken accessor in a ref inside the hook so
// `compute` is stable for the component's lifetime regardless of
// what the caller passes. This test enforces that contract: even
// when called with a fresh arrow on every render, the hook MUST
// NOT issue more `listBackends` requests than the operator-
// initiated path warrants (one on mount + one per focus event +
// one per 30s tick — none of which fire in a unit-test render).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, waitFor, act } from "@testing-library/react";
import { useVoiceReadiness } from "../chat/useVoiceReadiness";

let fetchMock: ReturnType<typeof vi.fn>;

function backendsResponse() {
    return new Response(
        JSON.stringify({
            backends: [
                {
                    purpose: "VoiceSTT",
                    configured: false,
                    backend: null,
                },
                {
                    purpose: "VoiceTTS",
                    configured: false,
                    backend: null,
                },
            ],
        }),
        { status: 200 },
    );
}

beforeEach(() => {
    localStorage.setItem("execlaw.access_token", "tok");
    localStorage.setItem("execlaw.refresh_token", "tok");
    fetchMock = vi.fn(async () => backendsResponse());
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
    localStorage.clear();
});

/** Wrapper that re-renders on every parent state tick + passes a
 *  brand-new arrow function to the hook every time — the exact
 *  shape WelcomeView used before the fix. */
function WelcomeShape({ tick }: { tick: number }) {
    // We WANT a fresh arrow on every render — that's what
    // reproduces the bug. `tick` is rendered as data-* so the
    // prop isn't dead code (TS6133-safe) and so the rerender's
    // wiring is observable to test assertions.
    const r = useVoiceReadiness(() =>
        localStorage.getItem("execlaw.access_token"),
    );
    return (
        <div data-testid="loading" data-tick={tick}>
            {r.loading ? "loading" : "ready"}
        </div>
    );
}

describe("useVoiceReadiness — render-loop regression", () => {
    it("does NOT spam listBackends when re-rendered with a fresh arrow each time", async () => {
        const { rerender, getByTestId } = render(<WelcomeShape tick={0} />);
        // Wait for the initial fetch to land + the `loading: true → false`
        // setState to flush.
        await waitFor(() => {
            expect(getByTestId("loading").textContent).toBe("ready");
        });
        // Force ten parent re-renders. With the old code each re-render
        // would (a) recreate `compute`, (b) re-fire `useEffect([compute])`,
        // (c) issue another listBackends call. With the fix, `compute` is
        // stable and the effect never re-fires.
        for (let i = 1; i <= 10; i++) {
            await act(async () => {
                rerender(<WelcomeShape tick={i} />);
            });
        }
        // Each compute() actually issues 1 fetch (listBackends only —
        // the unconfigured backends short-circuit before the per-purpose
        // status calls). Initial mount + StrictMode double-mount in dev
        // → up to 2 calls. Anything past 3 is a regression.
        expect(fetchMock.mock.calls.length).toBeLessThanOrEqual(3);
    });

    it("recovers from a successful fetch without queuing more on every render", async () => {
        const { rerender, getByTestId } = render(<WelcomeShape tick={0} />);
        await waitFor(() => {
            expect(getByTestId("loading").textContent).toBe("ready");
        });
        const callsAfterMount = fetchMock.mock.calls.length;
        // Re-render twenty times in quick succession.
        for (let i = 0; i < 20; i++) {
            await act(async () => {
                rerender(<WelcomeShape tick={i + 1} />);
            });
        }
        // Zero additional fetches: the hook ignores parent-render
        // churn entirely.
        expect(fetchMock.mock.calls.length).toBe(callsAfterMount);
    });
});
