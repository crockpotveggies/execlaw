// Tests for the auto-dismissing ErrorBanner component (2026-04-28).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { ErrorBanner } from "../components/ErrorBanner";

beforeEach(() => {
    vi.useFakeTimers();
});

afterEach(() => {
    vi.useRealTimers();
});

describe("ErrorBanner", () => {
    it("renders nothing when message is null", () => {
        render(<ErrorBanner message={null} onDismiss={() => {}} />);
        expect(screen.queryByTestId("error-banner")).toBeNull();
    });

    it("renders the message and a dismiss button", () => {
        render(
            <ErrorBanner message="something broke" onDismiss={() => {}} />,
        );
        const banner = screen.getByTestId("error-banner");
        expect(banner.textContent).toContain("something broke");
        expect(screen.getByTestId("error-banner-dismiss")).toBeInTheDocument();
    });

    it("calls onDismiss after the default 20 s timeout fires", () => {
        const onDismiss = vi.fn();
        render(
            <ErrorBanner
                message="server error"
                onDismiss={onDismiss}
            />,
        );
        // Just before 20 s — still showing.
        act(() => {
            vi.advanceTimersByTime(19_999);
        });
        expect(onDismiss).not.toHaveBeenCalled();
        // Cross the 20 s mark.
        act(() => {
            vi.advanceTimersByTime(2);
        });
        expect(onDismiss).toHaveBeenCalledTimes(1);
    });

    it("respects a custom dismissAfterMs", () => {
        const onDismiss = vi.fn();
        render(
            <ErrorBanner
                message="server error"
                onDismiss={onDismiss}
                dismissAfterMs={500}
            />,
        );
        act(() => {
            vi.advanceTimersByTime(501);
        });
        expect(onDismiss).toHaveBeenCalledTimes(1);
    });

    it("dismissAfterMs={0} disables auto-dismiss", () => {
        const onDismiss = vi.fn();
        render(
            <ErrorBanner
                message="permanent banner"
                onDismiss={onDismiss}
                dismissAfterMs={0}
            />,
        );
        // Even after a long wall-clock-ish window, no fire.
        act(() => {
            vi.advanceTimersByTime(10 * 60 * 1000);
        });
        expect(onDismiss).not.toHaveBeenCalled();
    });

    it("manual × button calls onDismiss immediately", () => {
        const onDismiss = vi.fn();
        render(
            <ErrorBanner message="oops" onDismiss={onDismiss} />,
        );
        fireEvent.click(screen.getByTestId("error-banner-dismiss"));
        expect(onDismiss).toHaveBeenCalledTimes(1);
    });

    it("a fresh message resets the countdown — the previous one's clock doesn't cut it short", () => {
        const onDismiss = vi.fn();
        const { rerender } = render(
            <ErrorBanner message="first" onDismiss={onDismiss} />,
        );
        // Burn 18 s on the first message.
        act(() => {
            vi.advanceTimersByTime(18_000);
        });
        rerender(<ErrorBanner message="second" onDismiss={onDismiss} />);
        // 5 more seconds (23 s total wall-clock, but only 5 s on the
        // refreshed timer) — must NOT fire.
        act(() => {
            vi.advanceTimersByTime(5_000);
        });
        expect(onDismiss).not.toHaveBeenCalled();
        // Cross the 20 s mark on the new countdown.
        act(() => {
            vi.advanceTimersByTime(15_001);
        });
        expect(onDismiss).toHaveBeenCalledTimes(1);
    });

    it("clears its timer on unmount so a stale fire never lands", () => {
        const onDismiss = vi.fn();
        const { unmount } = render(
            <ErrorBanner message="temp" onDismiss={onDismiss} />,
        );
        unmount();
        act(() => {
            vi.advanceTimersByTime(60_000);
        });
        expect(onDismiss).not.toHaveBeenCalled();
    });

    it("appends caller-supplied className alongside the base banner class", () => {
        render(
            <ErrorBanner
                message="hi"
                onDismiss={() => {}}
                className="mb-3"
            />,
        );
        const banner = screen.getByTestId("error-banner");
        expect(banner.className).toContain("execlaw-error-banner");
        expect(banner.className).toContain("mb-3");
    });

    it("honours custom testId for callsite-specific assertions", () => {
        render(
            <ErrorBanner
                message="login failed"
                onDismiss={() => {}}
                testId="login-error"
            />,
        );
        expect(screen.getByTestId("login-error")).toBeInTheDocument();
    });
});
