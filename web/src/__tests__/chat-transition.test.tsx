// Unit tests for `useChatTransition` — the GSAP transition that
// animates the WelcomeView → ActiveThreadPane handoff on first send.
//
// We don't mount the real `<Chat>` here; that brings in routing, the
// WS client, auth, the chat store, etc. Instead we drive the hook
// through a tiny harness that mirrors the contract:
//
//   <Harness>
//     {!hasContent && <div data-flip-id="composer-shell" />}  ← welcome stand-in
//     {hasContent && <div data-flip-id="composer-shell" />}   ← active stand-in
//     <button onClick={triggerSend}>send</button>
//   </Harness>
//
// The button calls `captureBeforeFirstSend()` then flips `hasContent`
// to true — same shape as the real ChatPane onSend wrapper.
//
// jsdom does NOT run layout, so getBoundingClientRect returns a
// zero-sized DOMRect by default. We override it on the stand-in
// elements so the hook can compute a meaningful dx/dy and reach
// the gsap.fromTo call path. The GSAP mock in setup.ts fires
// timeline-level onComplete synchronously; the assertions exercise
// that lifecycle.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
    act,
    fireEvent,
    render,
    screen,
} from "@testing-library/react";
import { useState } from "react";
import gsap from "gsap";
import { useChatTransition } from "../anim/useChatTransition";

let fromToSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
    fromToSpy = vi.spyOn(
        gsap as unknown as { fromTo: (..._: unknown[]) => unknown },
        "fromTo",
    ) as unknown as ReturnType<typeof vi.fn>;
});

afterEach(() => {
    (fromToSpy as unknown as { mockRestore: () => void }).mockRestore();
});

/** Force getBoundingClientRect to return the supplied rect for any
 *  element that matches `selector`. Persists across renders because
 *  it's installed on the prototype-bypassing Element directly. */
function stampRect(
    selector: string,
    rect: { top: number; left: number; width: number; height: number },
) {
    const el = document.querySelector(selector);
    if (!el) return;
    Object.defineProperty(el, "getBoundingClientRect", {
        configurable: true,
        value: () => ({
            top: rect.top,
            left: rect.left,
            width: rect.width,
            height: rect.height,
            right: rect.left + rect.width,
            bottom: rect.top + rect.height,
            x: rect.left,
            y: rect.top,
            toJSON: () => ({}),
        }),
    });
}

function Harness({ onComplete }: { onComplete?: () => void }) {
    const [hasContent, setHasContent] = useState(false);
    const { captureBeforeFirstSend } = useChatTransition({
        hasContent,
        onComplete,
    });

    const triggerSend = () => {
        if (!hasContent) {
            // Stamp the welcome composer's rect BEFORE capture so
            // the snapshot has meaningful values. Real layout would
            // place it ~vertical-center; we use 200x200 at (200,400).
            stampRect('[data-flip-id="composer-shell"]', {
                top: 400,
                left: 200,
                width: 200,
                height: 60,
            });
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
    it("captures the welcome composer's rect on captureBeforeFirstSend", () => {
        render(<Harness />);
        // Stamp the welcome composer's rect first so capture has
        // something to read; click the button to invoke capture.
        stampRect('[data-flip-id="composer-shell"]', {
            top: 400,
            left: 200,
            width: 200,
            height: 60,
        });
        // Spy on getBoundingClientRect by querying it AFTER capture
        // — easier than stubbing it; the assertions below check the
        // downstream tween fires, which proves the capture landed.
        act(() => {
            fireEvent.click(screen.getByTestId("trigger-send"));
        });
        // The welcome composer is now unmounted; the active composer
        // exists with the same data-flip-id. The hook's
        // useLayoutEffect should have computed a non-zero delta and
        // called gsap.fromTo on the new element.
        expect(fromToSpy).toHaveBeenCalled();
    });

    it("triggers gsap.fromTo with non-zero dx/dy on hasContent flip", () => {
        const onComplete = vi.fn();
        render(<Harness onComplete={onComplete} />);

        // Stamp ONLY the welcome composer with a defined position
        // BEFORE the click. After the click, the new active
        // composer's rect is whatever jsdom defaults to (zeros);
        // dx = saved.left - 0 = 200, dy = saved.top - 0 = 400.
        stampRect('[data-flip-id="composer-shell"]', {
            top: 400,
            left: 200,
            width: 200,
            height: 60,
        });
        act(() => {
            fireEvent.click(screen.getByTestId("trigger-send"));
        });
        expect(fromToSpy).toHaveBeenCalledTimes(1);
        // Inspect the call args: first call's second argument is the
        // `from` vars — should contain non-zero `x` and `y`.
        const fromVars = fromToSpy.mock.calls[0]?.[1] as {
            x: number;
            y: number;
        };
        expect(fromVars).toBeTruthy();
        expect(Math.abs(fromVars.x) + Math.abs(fromVars.y)).toBeGreaterThan(0);
        // Animation completes synchronously under the mock; the
        // caller's onComplete fires once.
        expect(onComplete).toHaveBeenCalledTimes(1);
    });

    it("does NOT animate when hasContent is already true at mount", () => {
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
        expect(fromToSpy).not.toHaveBeenCalled();
    });

    it("doesn't animate when capture is bypassed (e.g. deep-link nav)", () => {
        // Render in welcome state, flip hasContent without first
        // capturing. The hook should detect the missing snapshot
        // and skip the animation.
        function NoCaptureHarness() {
            const [hasContent, setHasContent] = useState(false);
            useChatTransition({ hasContent });
            return (
                <div>
                    {!hasContent && (
                        <div data-flip-id="composer-shell" data-testid="welcome">
                            welcome
                        </div>
                    )}
                    {hasContent && (
                        <div data-flip-id="composer-shell" data-testid="active">
                            active
                        </div>
                    )}
                    <button
                        type="button"
                        data-testid="flip"
                        onClick={() => setHasContent(true)}
                    >
                        flip
                    </button>
                </div>
            );
        }
        render(<NoCaptureHarness />);
        act(() => {
            fireEvent.click(screen.getByTestId("flip"));
        });
        expect(fromToSpy).not.toHaveBeenCalled();
    });

    it("honours prefers-reduced-motion: opacity-only fade, no translation, onComplete still fires", () => {
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
        stampRect('[data-flip-id="composer-shell"]', {
            top: 400,
            left: 200,
            width: 200,
            height: 60,
        });
        act(() => {
            fireEvent.click(screen.getByTestId("trigger-send"));
        });
        // Reduced-motion path tweens opacity on the composer + chrome
        // (two fromTo calls). Neither should include x/y translation.
        expect(fromToSpy).toHaveBeenCalledTimes(2);
        for (const call of fromToSpy.mock.calls) {
            const fromVars = call[1] as Record<string, unknown>;
            const toVars = call[2] as Record<string, unknown>;
            expect(fromVars.x).toBeUndefined();
            expect(fromVars.y).toBeUndefined();
            expect(toVars.x).toBeUndefined();
            expect(toVars.y).toBeUndefined();
        }
        expect(onComplete).toHaveBeenCalledTimes(1);
        mqSpy.mockRestore();
    });
});
