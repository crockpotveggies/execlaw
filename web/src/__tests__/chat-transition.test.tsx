// Unit tests for `useChatTransition` — the GSAP Flip transition that
// animates the WelcomeView → ActiveThreadPane handoff on first send.
//
// We don't mount the real `<Chat>` here; that brings in routing, the
// WS client, auth, the chat store, etc. Instead we drive the hook
// through a tiny harness that mirrors the contract:
//
//   <Harness>
//     <div data-flip-id="composer-shell" />   ← stand-in welcome composer
//     <button onClick={triggerSend} />         ← simulates onSend wrapper
//     {hasContent && <div data-flip-id="composer-shell" />}  ← stand-in active composer
//   </Harness>
//
// The button calls `captureBeforeFirstSend()` then flips `hasContent`
// to true — same shape as the real onSend wrapper inside ChatPane.
//
// Assertions exercise the GSAP mock in setup.ts:
//   * `Flip.getState` runs synchronously when capture is invoked.
//   * `Flip.from` runs synchronously when `hasContent` flips true.
//   * `onComplete` fires (lets the ChatPane focus the new textarea).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
    act,
    fireEvent,
    render,
    screen,
} from "@testing-library/react";
import { useState } from "react";
import { Flip } from "gsap/Flip";
import { useChatTransition } from "../anim/useChatTransition";

// Spy on the mocked Flip module so we can assert call shapes.
// Flip's static methods are typed as class members, which doesn't
// fit vi.spyOn's keyof object constraint cleanly. Cast to a plain
// indexable shape; the runtime mock from setup.ts is what executes.
const FlipForSpy = Flip as unknown as {
    getState: (..._: unknown[]) => unknown;
    from: (..._: unknown[]) => unknown;
};
let getStateSpy: ReturnType<typeof vi.fn>;
let fromSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
    getStateSpy = vi.spyOn(FlipForSpy, "getState") as unknown as ReturnType<
        typeof vi.fn
    >;
    fromSpy = vi.spyOn(FlipForSpy, "from") as unknown as ReturnType<
        typeof vi.fn
    >;
});

afterEach(() => {
    (getStateSpy as unknown as { mockRestore: () => void }).mockRestore();
    (fromSpy as unknown as { mockRestore: () => void }).mockRestore();
});

function Harness({
    onComplete,
}: {
    onComplete?: () => void;
}) {
    const [hasContent, setHasContent] = useState(false);
    const { captureBeforeFirstSend } = useChatTransition({
        hasContent,
        onComplete,
    });

    const triggerSend = () => {
        if (!hasContent) {
            captureBeforeFirstSend();
        }
        setHasContent(true);
    };

    return (
        <div>
            {!hasContent && (
                <div
                    data-flip-id="composer-shell"
                    data-testid="welcome-composer"
                >
                    welcome composer
                </div>
            )}
            {hasContent && (
                <>
                    <div className="execlaw-main__head" data-testid="active-head">
                        Header
                    </div>
                    <div
                        className="execlaw-stream-wrap"
                        data-testid="active-stream"
                    >
                        Stream
                    </div>
                    <div
                        data-flip-id="composer-shell"
                        data-testid="active-composer"
                    >
                        active composer
                    </div>
                </>
            )}
            <button
                type="button"
                data-testid="trigger-send"
                onClick={triggerSend}
            >
                send
            </button>
        </div>
    );
}

describe("useChatTransition", () => {
    it("captureBeforeFirstSend snapshots the welcome composer's Flip state", () => {
        render(<Harness />);
        expect(getStateSpy).not.toHaveBeenCalled();

        act(() => {
            fireEvent.click(screen.getByTestId("trigger-send"));
        });

        // First call: synchronous Flip.getState during the click
        // handler (BEFORE setState flips hasContent).
        expect(getStateSpy).toHaveBeenCalledWith(
            expect.anything(),
        );
        // The captured target was the welcome composer (only
        // [data-flip-id="composer-shell"] element on screen at
        // capture time).
        const callArg = getStateSpy.mock.calls[0]?.[0];
        // jsdom returns the matched element; we just confirm
        // something was passed.
        expect(callArg).toBeTruthy();
    });

    it("animates Flip.from when hasContent flips false → true with a pending snapshot", () => {
        const onComplete = vi.fn();
        render(<Harness onComplete={onComplete} />);

        act(() => {
            fireEvent.click(screen.getByTestId("trigger-send"));
        });

        // After the click, React commits hasContent=true. The
        // hook's useLayoutEffect picks up the pending snapshot and
        // dispatches Flip.from. The mocked Flip fires onComplete
        // synchronously, which the hook wraps in its timeline's
        // onComplete callback — that calls the caller's onComplete.
        expect(fromSpy).toHaveBeenCalledTimes(1);
        // Caller's onComplete fires once the timeline finishes.
        expect(onComplete).toHaveBeenCalledTimes(1);
    });

    it("does NOT run Flip.from when hasContent is already true at mount", () => {
        // Direct deep-link: an Active conversation is already showing
        // when ChatPane mounts. No prior welcome view → no captured
        // snapshot → hook is a no-op.
        function PreflippedHarness() {
            const [hasContent] = useState(true);
            useChatTransition({ hasContent });
            return (
                <div data-flip-id="composer-shell" data-testid="active-composer">
                    active composer
                </div>
            );
        }
        render(<PreflippedHarness />);
        expect(fromSpy).not.toHaveBeenCalled();
    });

    it("doesn't capture when called while already in active mode", () => {
        // hasContent is already true at the time captureBeforeFirstSend
        // would be invoked from inside ChatPane's onSend wrapper. The
        // wrapper's `if (!hasContent)` guard prevents the call; this
        // test confirms that bypassing the guard (calling capture
        // anyway) doesn't break the contract — the next send doesn't
        // mistakenly trigger the timeline because hasContent is
        // already true.
        function ActiveHarness() {
            const [hasContent] = useState(true);
            const { captureBeforeFirstSend } = useChatTransition({
                hasContent,
            });
            return (
                <div>
                    <div
                        data-flip-id="composer-shell"
                        data-testid="active-composer"
                    >
                        active composer
                    </div>
                    <button
                        type="button"
                        data-testid="capture"
                        onClick={() => captureBeforeFirstSend()}
                    >
                        capture
                    </button>
                </div>
            );
        }
        render(<ActiveHarness />);
        act(() => {
            fireEvent.click(screen.getByTestId("capture"));
        });
        // The capture itself called Flip.getState, but no
        // hasContent flip happens, so the timeline never fires.
        expect(fromSpy).not.toHaveBeenCalled();
    });

    it("honours prefers-reduced-motion: skips Flip.from but still calls onComplete", () => {
        // Stub matchMedia to report reduced-motion. jsdom returns
        // a default-shape MediaQueryList; we override .matches.
        const mqSpy = vi.spyOn(window, "matchMedia").mockImplementation(
            (q: string) =>
                ({
                    matches: q.includes("prefers-reduced-motion"),
                    media: q,
                    onchange: null,
                    addEventListener: () => {},
                    removeEventListener: () => {},
                    addListener: () => {},
                    removeListener: () => {},
                    dispatchEvent: () => false,
                }) as unknown as MediaQueryList,
        );
        const onComplete = vi.fn();
        render(<Harness onComplete={onComplete} />);
        act(() => {
            fireEvent.click(screen.getByTestId("trigger-send"));
        });
        // Capture still runs (cheap — no animation cost).
        expect(getStateSpy).toHaveBeenCalled();
        // But Flip.from is skipped under reduced-motion.
        expect(fromSpy).not.toHaveBeenCalled();
        // The focus-retention callback still fires so the operator
        // doesn't lose their input position.
        expect(onComplete).toHaveBeenCalledTimes(1);
        mqSpy.mockRestore();
    });
});
