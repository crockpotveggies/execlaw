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

// jsdom 26 (vitest 4 default) no longer ships a `matchMedia` stub;
// tests that spy on it via `vi.spyOn(window, "matchMedia")` fail
// with "can only spy on a function". Provide a minimal default the
// per-test overrides can replace.
if (typeof window !== "undefined" && typeof window.matchMedia !== "function") {
    Object.defineProperty(window, "matchMedia", {
        configurable: true,
        writable: true,
        value: (query: string) => ({
            matches: false,
            media: query,
            onchange: null,
            addListener: () => {},
            removeListener: () => {},
            addEventListener: () => {},
            removeEventListener: () => {},
            dispatchEvent: () => false,
        }),
    });
}

vi.mock("gsap", () => {
    type Vars = Record<string, unknown> & { onComplete?: () => void };
    const fire = (vars: Vars | undefined) => {
        if (vars && typeof vars.onComplete === "function") vars.onComplete();
        return { kill() {}, then: () => Promise.resolve() };
    };
    // Chainable timeline stub. Every `.add/.to/.from/.fromTo` returns
    // the same instance so the call sites can still chain. Each
    // per-tween call fires THAT tween's `vars.onComplete` (matching
    // the real GSAP contract); the timeline-level `cfg.onComplete`
    // fires exactly once when the factory is called, simulating
    // "all tweens finished" under jsdom's synchronous test loop.
    const timeline = (cfg?: Vars) => {
        const tl = {
            add: (_target: unknown, _at?: number) => tl,
            to: (_t: unknown, vars?: Vars, _at?: number) => {
                fire(vars);
                return tl;
            },
            from: (_t: unknown, vars?: Vars, _at?: number) => {
                fire(vars);
                return tl;
            },
            fromTo: (
                _t: unknown,
                _f: Vars | undefined,
                vars?: Vars,
                _at?: number,
            ) => {
                fire(vars);
                return tl;
            },
            // `set` is a synchronous state set; no tween, no
            // onComplete to fire — just chain.
            set: (_t: unknown, _vars?: Vars, _at?: number) => tl,
            kill() {},
            then: () => Promise.resolve(),
        };
        fire(cfg);
        return tl;
    };
    const gsap = {
        to: (_t: unknown, vars?: Vars) => fire(vars),
        from: (_t: unknown, vars?: Vars) => fire(vars),
        fromTo: (_t: unknown, _f: Vars | undefined, vars?: Vars) => fire(vars),
        set: () => ({ kill() {} }),
        // `quickTo` returns a setter function. In production it tweens
        // the property smoothly; under the mock we just no-op so call
        // sites that use `xTo(value)` don't crash.
        quickTo: (_target: unknown, _prop: string, _vars?: Vars) => {
            const setter = (_value: number) => setter;
            return setter;
        },
        registerPlugin: () => {},
        context: (fn: () => void) => {
            try {
                fn();
            } catch {
                /* ignore */
            }
            return { kill() {}, revert() {} };
        },
        timeline,
    };
    return { default: gsap, gsap };
});

// GSAP Flip plugin mock. `getState` returns a sentinel object; `from`
// is treated like a regular tween — fires `onComplete` synchronously.
// Real Flip captures DOM rects and animates transforms; tests only
// need to confirm the call shape (was `getState` invoked? was
// `from` invoked with the saved state?), so a stub is sufficient.
vi.mock("gsap/Flip", () => {
    type Vars = Record<string, unknown> & { onComplete?: () => void };
    const Flip = {
        getState: (_targets: unknown) => ({
            __flipMockState: true,
            targets: _targets,
        }),
        from: (_state: unknown, vars?: Vars) => {
            if (vars && typeof vars.onComplete === "function") {
                vars.onComplete();
            }
            return { kill() {}, then: () => Promise.resolve() };
        },
    };
    return { default: Flip, Flip };
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
