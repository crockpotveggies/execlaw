import { describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useDismissAnimation } from "../anim/useDismissAnimation";

describe("useDismissAnimation", () => {
    it("returns a style object and a dismiss callback", () => {
        const { result } = renderHook(() => useDismissAnimation());
        expect(result.current.style).toBeTypeOf("object");
        expect(typeof result.current.dismiss).toBe("function");
    });

    it("dismiss runs the `after` callback after the timing animations complete", () => {
        const after = vi.fn();
        const { result } = renderHook(() => useDismissAnimation());
        act(() => {
            result.current.dismiss(after);
        });
        // The mocked withTiming fires the completion callback synchronously
        // (see __tests__/setup.ts), so the after callback has already run.
        expect(after).toHaveBeenCalledTimes(1);
    });

    it("dismiss without an `after` callback doesn't throw", () => {
        const { result } = renderHook(() => useDismissAnimation());
        expect(() => {
            act(() => {
                result.current.dismiss();
            });
        }).not.toThrow();
    });

    it("custom toScale + toOpacity options accepted without error", () => {
        const { result } = renderHook(() =>
            useDismissAnimation({ toOpacity: 0.2, toScale: 1, durationMs: 100 }),
        );
        expect(() => {
            act(() => {
                result.current.dismiss();
            });
        }).not.toThrow();
    });
});
