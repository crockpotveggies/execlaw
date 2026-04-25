// vitest setup — extends `expect` with @testing-library/jest-dom matchers
// and resets DOM/storage between tests.
//
// Storage shim: Node ≥ 22 ships a built-in `localStorage` that lacks
// methods like `clear()` and disagrees with the spec in subtle ways.
// On test boot we install a Map-backed Storage implementation that
// works under both jsdom and Node-native environments.
//
// GSAP mock: gsap.to/gsap.from/etc are stubbed to fire `onComplete`
// synchronously so animation hooks resolve instantly under jsdom.
// We don't care about visual changes in unit tests; we just need the
// JS lifecycle (in particular: navigate-after-dismiss) to advance.

import "@testing-library/jest-dom/vitest";
import { afterEach, vi } from "vitest";
import { cleanup } from "@testing-library/react";

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

vi.mock("gsap", () => {
    type Vars = Record<string, unknown> & { onComplete?: () => void };
    const fire = (vars: Vars | undefined) => {
        if (vars && typeof vars.onComplete === "function") vars.onComplete();
        return { kill() {}, then: () => Promise.resolve() };
    };
    const gsap = {
        to: (_t: unknown, vars?: Vars) => fire(vars),
        from: (_t: unknown, vars?: Vars) => fire(vars),
        fromTo: (_t: unknown, _f: Vars | undefined, vars?: Vars) => fire(vars),
        set: () => ({ kill() {} }),
        registerPlugin: () => {},
        context: (fn: () => void) => {
            try {
                fn();
            } catch {
                /* ignore */
            }
            return { kill() {}, revert() {} };
        },
    };
    return { default: gsap, gsap };
});

vi.mock("@gsap/react", () => ({
    useGSAP: (fn: () => void | (() => void)) => {
        // Run synchronously — like a useEffect with no deps that
        // doesn't unsubscribe in tests.
        try {
            const cleanup = fn();
            if (typeof cleanup === "function") {
                // Tests don't unmount via useGSAP cleanup; ignore.
            }
        } catch {
            /* ignore */
        }
    },
}));

afterEach(() => {
    (globalThis.localStorage as MapStorage).clear();
    cleanup();
});
