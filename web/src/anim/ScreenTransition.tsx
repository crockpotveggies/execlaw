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
} from "react-native-reanimated";

interface Props {
    children: ReactNode;
    /** Delay before the animation begins, in ms. */
    delayMs?: number;
}

export function ScreenTransition({ children, delayMs = 0 }: Props) {
    // 220ms feels snappy on a 60Hz monitor without looking instant.
    // 8px lift gives the eye something to anchor on without
    // crossing into "stagey" territory.
    const animation = FadeInUp.duration(220)
        .delay(delayMs)
        .easing(Easing.out(Easing.cubic));

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
