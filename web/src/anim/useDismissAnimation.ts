// useDismissAnimation
//
// Hook that returns:
//   - `style` — a Reanimated animated style with current opacity + scale
//   - `dismiss(after)` — kicks off the dismissal animation; calls `after`
//     once the animation finishes.
//
// Use to choreograph a route's exit animation BEFORE navigating away,
// so React Router doesn't synchronously unmount the component.
//
// Tuned for the login → chat handoff: 280ms is short enough to feel
// snappy on the happy path but long enough that the eye actually sees
// the form shrink rather than vanishing instantly.

import { useCallback } from "react";
import {
    runOnJS,
    useAnimatedStyle,
    useSharedValue,
    withTiming,
} from "react-native-reanimated";

export interface DismissOptions {
    /** Final opacity. Defaults to 0. */
    toOpacity?: number;
    /** Final scale. Defaults to 0.85 (gentle shrink). */
    toScale?: number;
    /** Duration in ms. Defaults to 280. */
    durationMs?: number;
}

export function useDismissAnimation(opts: DismissOptions = {}) {
    const { toOpacity = 0, toScale = 0.85, durationMs = 280 } = opts;

    const opacity = useSharedValue(1);
    const scale = useSharedValue(1);

    const style = useAnimatedStyle(() => ({
        opacity: opacity.value,
        transform: [{ scale: scale.value }],
    }));

    const dismiss = useCallback(
        (after?: () => void) => {
            opacity.value = withTiming(toOpacity, { duration: durationMs });
            scale.value = withTiming(
                toScale,
                { duration: durationMs },
                (finished) => {
                    "worklet";
                    if (finished && after) {
                        runOnJS(after)();
                    }
                },
            );
        },
        [opacity, scale, toOpacity, toScale, durationMs],
    );

    return { style, dismiss };
}
