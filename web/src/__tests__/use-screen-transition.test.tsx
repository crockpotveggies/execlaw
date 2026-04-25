import { describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useScreenTransition } from "../anim/useScreenTransition";

describe("useScreenTransition", () => {
    it("returns a ref + a dismiss function", () => {
        const { result } = renderHook(() => useScreenTransition());
        expect(result.current.ref).toBeTypeOf("object");
        expect(typeof result.current.dismiss).toBe("function");
    });

    it("dismiss without a mounted element invokes the after callback immediately", () => {
        const after = vi.fn();
        const { result } = renderHook(() => useScreenTransition());
        act(() => {
            result.current.dismiss(after);
        });
        expect(after).toHaveBeenCalledTimes(1);
    });

    it("dismiss with a mounted element fires onComplete via the GSAP mock", () => {
        const after = vi.fn();
        const { result } = renderHook(() => useScreenTransition<HTMLDivElement>());
        // Simulate a real React ref attachment so the hook has something to animate.
        const div = document.createElement("div");
        document.body.appendChild(div);
        result.current.ref.current = div;
        act(() => {
            result.current.dismiss(after);
        });
        // The mocked gsap.to fires onComplete synchronously
        // (see src/__tests__/setup.ts).
        expect(after).toHaveBeenCalledTimes(1);
        document.body.removeChild(div);
    });

    it("dismiss without an after callback does not throw", () => {
        const { result } = renderHook(() => useScreenTransition<HTMLDivElement>());
        const div = document.createElement("div");
        result.current.ref.current = div;
        expect(() => {
            act(() => {
                result.current.dismiss();
            });
        }).not.toThrow();
    });

    it("custom config (chat shell: pure fade) doesn't throw", () => {
        const { result } = renderHook(() =>
            useScreenTransition<HTMLDivElement>({
                initialScale: 1,
                exitScale: 1,
                durationMs: 240,
            }),
        );
        const div = document.createElement("div");
        result.current.ref.current = div;
        expect(() => {
            act(() => {
                result.current.dismiss();
            });
        }).not.toThrow();
    });
});
