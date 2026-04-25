// Tiny Reanimated 4 wrapper that fades + lifts a route on mount.
//
// We use Reanimated 4's CSS-animation web target, so on browsers this
// compiles down to native `animation: ...` and runs on the compositor —
// no JS work per frame. On native (iOS/Android, post-Phase-6e) the
// same component drives a worklet-backed animation on the UI thread.
//
// Usage: wrap a route's root element.
//
//   <ScreenTransition>
//       <SetupWizard />
//   </ScreenTransition>
//
// Keep the API minimal: just `children` + an optional delay. Per-route
// custom shapes (slide-in from the side, scale, etc.) layer on later
// once we actually want them.

import { type ReactNode } from "react";
import Animated, {
    Easing,
    FadeInUp,
    ZoomIn,
} from "react-native-reanimated";

/**
 * Visual character of the entry animation.
 *
 *   - "fade" (default): subtle fade-up. Used for the chat shell —
 *     content scrolls in feeling like it appeared.
 *   - "zoom": scale 0.85 → 1.0 + opacity 0 → 1. Used for auth screens
 *     so the login card feels like it grows into place; pairs with
 *     the dismiss animation that shrinks it back on a successful sign-in.
 */
export type ScreenTransitionKind = "fade" | "zoom";

interface Props {
    children: ReactNode;
    /** Delay before the animation begins, in ms. */
    delayMs?: number;
    kind?: ScreenTransitionKind;
}

export function ScreenTransition({
    children,
    delayMs = 0,
    kind = "fade",
}: Props) {
    const easing = Easing.out(Easing.cubic);
    const animation =
        kind === "zoom"
            ? ZoomIn.duration(280).delay(delayMs).easing(easing)
            : FadeInUp.duration(220).delay(delayMs).easing(easing);

    return (
        <Animated.View
            entering={animation}
            // The layout is owned by the wrapped route; this view is
            // a transparent layer that occupies the full available
            // box so the route's existing CSS keeps working.
            style={{ flex: 1, width: "100%", height: "100%" }}
        >
            {children}
        </Animated.View>
    );
}
