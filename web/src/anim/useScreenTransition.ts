// useScreenTransition
//
// One hook that drives BOTH the entry and exit of a route, using GSAP.
//
// Returns:
//   - `ref` — attach this to the element you want animated.
//   - `dismiss(after?)` — kicks off the exit animation; calls `after`
//     once the tween completes (or immediately if the ref isn't mounted).
//
// On mount: GSAP animates the element FROM (initialOpacity, initialScale)
// to its inline state (opacity 1, scale 1). On dismiss: GSAP animates it
// TO (exitOpacity, exitScale).
//
// Defaults match the auth-card flow: scale 0.85 + opacity 0 at both
// ends. Pass `{ initialScale: 1, exitScale: 1 }` for a pure fade.

import { useCallback, useRef } from "react";
import { useGSAP } from "@gsap/react";
import gsap from "gsap";

export interface ScreenTransitionConfig {
    /** Opacity the element starts at on mount. Animates UP to 1. */
    initialOpacity?: number;
    /** Scale the element starts at on mount. Animates UP to 1. */
    initialScale?: number;
    /** Opacity the element ends at on dismiss. */
    exitOpacity?: number;
    /** Scale the element ends at on dismiss. */
    exitScale?: number;
    /** Duration of both animations in ms. */
    durationMs?: number;
    /** GSAP ease string. Defaults to a snappy `power3.out`. */
    ease?: string;
}

export function useScreenTransition<T extends HTMLElement = HTMLDivElement>(
    cfg: ScreenTransitionConfig = {},
) {
    const {
        initialOpacity = 0,
        initialScale = 0.85,
        exitOpacity = 0,
        exitScale = 0.85,
        durationMs = 280,
        ease = "power3.out",
    } = cfg;

    const ref = useRef<T | null>(null);
    const durationS = durationMs / 1000;

    useGSAP(
        () => {
            const el = ref.current;
            if (!el) return;
            gsap.from(el, {
                opacity: initialOpacity,
                scale: initialScale,
                duration: durationS,
                ease,
            });
        },
        // Empty deps — only run once on mount, like a fade-in on first paint.
        { dependencies: [] },
    );

    const dismiss = useCallback(
        (after?: () => void) => {
            const el = ref.current;
            if (!el) {
                after?.();
                return;
            }
            gsap.to(el, {
                opacity: exitOpacity,
                scale: exitScale,
                duration: durationS,
                ease,
                onComplete: after,
            });
        },
        [exitOpacity, exitScale, durationS, ease],
    );

    return { ref, dismiss };
}
