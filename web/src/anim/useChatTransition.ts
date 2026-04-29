// useChatTransition
//
// Drives the WelcomeView → ActiveThreadPane GSAP-Flip transition that
// fires when the operator sends their first message.
//
// CONTRACT
//   * `captureBeforeFirstSend()` is called from WelcomeView's onSend
//     wrapper, BEFORE delegating to the parent's `onSend(text)`. It
//     synchronously snapshots the current welcome composer's layout
//     so the upcoming render — which unmounts WelcomeView and mounts
//     ActiveThreadPane — has a "from" position to animate from.
//
//   * `runTransitionIfPending()` is called from a `useLayoutEffect`
//     whose deps include `hasContent`. When a snapshot is pending
//     AND `hasContent` just flipped true, we fire the timeline:
//       - Flip.from(snapshot, { absolute: true, ... }) on the new
//         composer. The composer visually starts where the welcome
//         composer was and animates to its natural bottom-anchored
//         position.
//       - A subtle `from` fade on the new active pane's header +
//         message stream so the chrome doesn't pop.
//       - On complete, focus the new composer's textarea so the
//         operator can keep typing without grabbing the input.
//
// REDUCED MOTION
//   `(prefers-reduced-motion: reduce)` skips the entire timeline and
//   leaves React's natural mount/unmount in place. The focus-retention
//   step still runs.
//
// The Flip + timeline only fire when the welcome composer was
// actually on screen — direct deep-link navigation to /chat/:id
// doesn't trigger a snapshot, so the hook is a no-op there.
//
// 2026-04-28.

import { useCallback, useLayoutEffect, useRef } from "react";
import gsap from "gsap";
import { Flip } from "gsap/Flip";

gsap.registerPlugin(Flip);

const FLIP_DURATION_S = 0.42;
const FADE_DURATION_S = 0.22;
const FADE_DELAY_S = 0.2;

/** Element selectors used by the timeline. Centralised so test fixtures
 *  + the scaffold + the animation body all agree on names. */
const SELECTORS = {
    composerShell: '[data-flip-id="composer-shell"]',
    activeChrome: '.execlaw-main__head, .execlaw-stream-wrap',
} as const;

/**
 * Returns `true` when the OS asks for reduced motion. SSR-safe.
 */
function prefersReducedMotion(): boolean {
    if (typeof window === "undefined" || !window.matchMedia) return false;
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

interface ChatTransitionState {
    /// Captured Flip state from the welcome composer's pre-send
    /// layout. Cleared once the transition fires (or skipped).
    pending: ReturnType<typeof Flip.getState> | null;
}

export function useChatTransition({
    hasContent,
    onComplete,
}: {
    hasContent: boolean;
    /// Optional hook for the chat shell to react to the animation
    /// finishing (e.g. focus the new textarea). Called even when
    /// reduced-motion skips the timeline.
    onComplete?: () => void;
}) {
    const stateRef = useRef<ChatTransitionState>({ pending: null });
    const prevHasContentRef = useRef(hasContent);

    /// Welcome view calls this from its `onSend` wrapper, BEFORE the
    /// parent's `onSend(text)` triggers the React state change that
    /// flips `hasContent`. Synchronous capture in the user-event
    /// handler means we still see the welcome composer's layout.
    const captureBeforeFirstSend = useCallback(() => {
        if (typeof document === "undefined") return;
        const target = document.querySelector(SELECTORS.composerShell);
        if (!target) return;
        try {
            stateRef.current.pending = Flip.getState(target);
        } catch {
            // Flip plugin not available / DOM mid-flight. Skip the
            // capture; the transition will be a hard cut.
            stateRef.current.pending = null;
        }
    }, []);

    /// Run the timeline if a snapshot is pending and `hasContent`
    /// just flipped from false → true. Idempotent — once consumed,
    /// the snapshot is cleared.
    useLayoutEffect(() => {
        const justFlippedTrue =
            prevHasContentRef.current === false && hasContent === true;
        prevHasContentRef.current = hasContent;

        if (!justFlippedTrue) return;
        const pending = stateRef.current.pending;
        if (!pending) return;
        stateRef.current.pending = null;

        if (prefersReducedMotion()) {
            // No animation; still let the caller react (focus the
            // new composer, etc.).
            onComplete?.();
            return;
        }

        const tl = gsap.timeline({
            onComplete: () => {
                onComplete?.();
            },
        });

        // Composer slides from welcome's center to active's bottom.
        // `absolute: true` makes Flip use absolute positioning during
        // the tween so the surrounding flex layout doesn't fight us.
        tl.add(
            Flip.from(pending, {
                duration: FLIP_DURATION_S,
                ease: "power3.inOut",
                absolute: true,
            }),
            0,
        );

        // Soft fade-in for the active pane's chrome (header + stream
        // wrapper). They mount with the new render; nudging opacity
        // 0 → 1 + a small y-translate makes them feel deliberate
        // rather than popping in.
        tl.from(
            SELECTORS.activeChrome,
            {
                opacity: 0,
                y: 8,
                duration: FADE_DURATION_S,
                ease: "power2.out",
            },
            FADE_DELAY_S,
        );
    }, [hasContent, onComplete]);

    return { captureBeforeFirstSend };
}
