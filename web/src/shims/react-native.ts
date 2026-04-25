// Drop-in shim aliased to `react-native` on the Vite + Vitest builds.
//
// react-native-web covers most of the public RN surface, but
// react-native-reanimated 4 reaches for `TurboModuleRegistry` and
// `NativeModules` at module load. Those don't exist on web. We re-
// export everything react-native-web ships, then add no-op stubs for
// the native-only registries — enough for Reanimated 4 to bootstrap
// in CSS-animation web mode without crashing the bundler.
//
// When iOS/Android targets land (Phase 6e+), the bundler config flips
// the alias back to the real `react-native` package and these stubs
// vanish.

// react-native-web ships its own types via `react-native` (it
// piggybacks on the upstream RN type packs), but `import * from`
// in a strict-TS project still needs a module declaration.
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error — RN-web has no shipped types module.
export * from "react-native-web";

/** No-op TurboModuleRegistry — every lookup returns null. */
export const TurboModuleRegistry = {
    get<T>(_name: string): T | null {
        return null;
    },
    getEnforcing<T>(name: string): T {
        throw new Error(
            `TurboModuleRegistry.getEnforcing called for "${name}" on web; \
this should not happen — Reanimated should fall back to its CSS path.`,
        );
    },
};

/** Empty NativeModules registry — Reanimated checks this for installation hints. */
export const NativeModules: Record<string, unknown> = {};

/** No-op NativeEventEmitter constructor. Returns an object whose `addListener`
 *  / `removeAllListeners` are also no-ops.
 */
export class NativeEventEmitter {
    addListener() {
        return { remove() {} };
    }
    removeAllListeners() {}
    emit() {}
}
