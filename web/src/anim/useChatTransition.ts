// useChatTransition
//
// Drives the WelcomeView → ActiveThreadPane GSAP transition that
// fires when the operator sends their first message.
//
// CONTRACT
//   * `captureBeforeFirstSend()` is called from ChatPane's onSend
//     wrapper, BEFORE delegating to the parent's `onSend(text)`. It
//     synchronously snapshots the welcome composer's bounding rect
//     so the upcoming render — which unmounts WelcomeView and
//     mounts ActiveThreadPane — has a "from" position to animate
//     from.
//
//   * The hook's `useLayoutEffect` watches `hasContent`. On the
//     false → true edge AND with a pending snapshot, it fires a
//     timeline:
//       - The new composer is gsap.fromTo()'d from the captured
//         rect (welcome's center) to its natural position
//         (active's bottom). We use `transform: translate(dx, dy)`
//         so we don't have to re-layout the element absolutely
//         during the tween — the surrounding flex layout stays
//         stable.
//       - A short fade on the active pane's chrome (header + stream
//         wrapper) so they don't pop into existence.
//       - On complete, the caller's `onComplete` runs (focus the
//         new composer's textarea, etc.).
//
// WHY NOT Flip.getState/Flip.from?
//   The previous version stored the welcome composer as an Element
//   reference and called Flip.from(savedState). Once WelcomeView
//   unmounted, that reference was dead and Flip's animation was a
//   no-op — the user saw a hard cut.
//
//   Switching to a plain rect capture + gsap.fromTo on the NEW
//   composer DOM node sidesteps Flip's element-tracking entirely.
//   The animation is straightforward: tween a transform from
//   `(dx, dy)` to `(0, 0)` on the element that exists post-swap.
//
// REDUCED MOTION
//   `(prefers-reduced-motion: reduce)` swaps the slide-tween for a
//   short opacity fade — no translation, but the operator still
//   gets a visual cue that the view changed. `onComplete` fires
//   either way so the focus-retention path runs.
//
// 2026-04-28.

import { useCallback, useLayoutEffect, useRef } from "react";
import gsap from "gsap";

const FLIP_DURATION_S = 0.42;
const FADE_DURATION_S = 0.22;
const FADE_DELAY_S = 0.2;

const SELECTORS = {
    composerShell: '[data-flip-id="composer-shell"]',
    activeChrome: ".execlaw-main__head, .execlaw-stream-wrap",
} as const;

interface SavedRect {
    top: number;
    left: number;
    width: number;
    height: number;
}

function prefersReducedMotion(): boolean {
    if (typeof window === "undefined" || !window.matchMedia) return false;
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export function useChatTransition({
    hasContent,
    onComplete,
}: {
    hasContent: boolean;
    onComplete?: () => void;
}) {
    const pendingRef = useRef<SavedRect | null>(null);
    const prevHasContentRef = useRef(hasContent);
    // Pin onComplete in a ref so the effect's dep array doesn't
    // re-fire on every parent render (parent might pass a fresh
    // arrow each time). The effect tracks hasContent only.
    const onCompleteRef = useRef(onComplete);
    onCompleteRef.current = onComplete;

    const captureBeforeFirstSend = useCallback(() => {
        if (typeof document === "undefined") return;
        const target = document.querySelector<HTMLElement>(
            SELECTORS.composerShell,
        );
        if (!target) {
            // eslint-disable-next-line no-console
            console.log(
                "[chat-transition] capture: no element matched",
                SELECTORS.composerShell,
            );
            return;
        }
        const rect = target.getBoundingClientRect();
        pendingRef.current = {
            top: rect.top,
            left: rect.left,
            width: rect.width,
            height: rect.height,
        };
        // eslint-disable-next-line no-console
        console.log("[chat-transition] capture", pendingRef.current);
    }, []);

    useLayoutEffect(() => {
        const justFlippedTrue =
            prevHasContentRef.current === false && hasContent === true;
        prevHasContentRef.current = hasContent;
        if (!justFlippedTrue) {
            // eslint-disable-next-line no-console
            console.log("[chat-transition] effect: not a false→true flip", {
                prev: !justFlippedTrue,
                hasContent,
            });
            return;
        }
        const saved = pendingRef.current;
        if (!saved) {
            // eslint-disable-next-line no-console
            console.log(
                "[chat-transition] effect: flipped true but no saved rect — capture path didn't run",
            );
            return;
        }
        // eslint-disable-next-line no-console
        console.log("[chat-transition] effect: starting animation", saved);
        pendingRef.current = null;

        const fireOnComplete = () => {
            try {
                onCompleteRef.current?.();
            } catch {
                /* swallow caller errors so the animation lifecycle stays clean */
            }
        };

        // Find the NEW composer (post-swap). It exists in the
        // active pane's tree, with the same data-flip-id.
        const newComposer = document.querySelector<HTMLElement>(
            SELECTORS.composerShell,
        );
        if (!newComposer) {
            // eslint-disable-next-line no-console
            console.log(
                "[chat-transition] effect: new composer not found post-swap",
            );
            fireOnComplete();
            return;
        }

        const reduced = prefersReducedMotion();
        // eslint-disable-next-line no-console
        console.log("[chat-transition] reduced-motion?", reduced);

        if (reduced) {
            // Honor the OS preference: no translation, but still a
            // brief fade so the operator gets a visual cue.
            gsap.fromTo(
                newComposer,
                { opacity: 0 },
                {
                    opacity: 1,
                    duration: FADE_DURATION_S,
                    ease: "power2.out",
                    onComplete: fireOnComplete,
                },
            );
            gsap.fromTo(
                SELECTORS.activeChrome,
                { opacity: 0 },
                {
                    opacity: 1,
                    duration: FADE_DURATION_S,
                    ease: "power2.out",
                },
            );
            return;
        }

        // Compute the translate delta.
        const newRect = newComposer.getBoundingClientRect();
        const dx = saved.left - newRect.left;
        const dy = saved.top - newRect.top;
        // eslint-disable-next-line no-console
        console.log("[chat-transition] delta", { dx, dy, newRect });

        if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5) {
            // eslint-disable-next-line no-console
            console.log(
                "[chat-transition] effect: no-op — composer didn't move",
            );
            fireOnComplete();
            return;
        }

        // Composer move: tween transform from (dx, dy) to (0, 0)
        // on the new DOM element. `onComplete` fires the caller's
        // callback (focus retention).
        gsap.fromTo(
            newComposer,
            { x: dx, y: dy, opacity: 0.95 },
            {
                x: 0,
                y: 0,
                opacity: 1,
                duration: FLIP_DURATION_S,
                ease: "power3.inOut",
                onComplete: fireOnComplete,
            },
        );
        // Chrome fade — starts FADE_DELAY_S into the composer move
        // so the header + stream don't pop in. Independent tween;
        // we don't observe its onComplete (the composer move's is
        // the load-bearing signal).
        gsap.from(SELECTORS.activeChrome, {
            opacity: 0,
            y: 8,
            duration: FADE_DURATION_S,
            delay: FADE_DELAY_S,
            ease: "power2.out",
        });
    }, [hasContent]);

    return { captureBeforeFirstSend };
}
