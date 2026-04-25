// vitest setup — extends `expect` with @testing-library/jest-dom matchers
// and resets DOM/storage between tests.
//
// Storage shim: Node ≥ 22 ships a built-in `localStorage` that lacks
// methods like `clear()` and disagrees with the spec in subtle ways.
// On test boot we install a Map-backed Storage implementation that
// works under both jsdom and Node-native environments. The behavior
// mirrors the real Storage API closely enough for our auth tests.

import "@testing-library/jest-dom/vitest";
import { afterEach, vi } from "vitest";
import { cleanup } from "@testing-library/react";

// Reanimated's native specs touch the TurboModule registry at import
// time, which doesn't exist under jsdom. Stub the surface we use: the
// `Animated.View` component, an `entering` prop pass-through, and the
// `FadeInUp` builder. Real animation behavior is exercised in the
// browser; tests only need the components to render their children.
vi.mock("react-native-reanimated", () => {
    const React = require("react") as typeof import("react");

    const passthrough = (
        tag: string,
    ) =>
        React.forwardRef<HTMLElement, Record<string, unknown>>(
            ({ children, style: _style, entering: _e, ...rest }, ref) =>
                React.createElement(tag, { ...rest, ref }, children as React.ReactNode),
        );

    const Animated = {
        View: passthrough("div"),
        Text: passthrough("span"),
    };

    const builder = () => ({
        duration: () => builder(),
        delay: () => builder(),
        easing: () => builder(),
    });

    return {
        default: Animated,
        Easing: {
            out: () => undefined,
            cubic: undefined,
        },
        FadeInUp: builder(),
        FadeIn: builder(),
        FadeOut: builder(),
    };
});

class MapStorage implements Storage {
    private map = new Map<string, string>();
    get length() {
        return this.map.size;
    }
    clear() {
        this.map.clear();
    }
    getItem(k: string) {
        return this.map.has(k) ? this.map.get(k)! : null;
    }
    key(i: number) {
        return Array.from(this.map.keys())[i] ?? null;
    }
    removeItem(k: string) {
        this.map.delete(k);
    }
    setItem(k: string, v: string) {
        this.map.set(k, String(v));
    }
}

Object.defineProperty(globalThis, "localStorage", {
    value: new MapStorage(),
    configurable: true,
    writable: true,
});

afterEach(() => {
    (globalThis.localStorage as MapStorage).clear();
    cleanup();
});
