// Auto-dismissing error banner.
//
// Replaces the bare
//   {error && (
//       <div className="execlaw-error-banner ..." role="alert">
//           {error}
//       </div>
//   )}
// pattern that was scattered across every settings page + the chat
// shell. Behaviour:
//
//   * When `message` is non-null, render a banner with the existing
//     `execlaw-error-banner` styling.
//   * Start a 20s timeout (configurable via `dismissAfterMs`); when
//     it fires, call `onDismiss` so the parent's `setError(null)`
//     clears the message and the banner unmounts.
//   * The timer resets every time `message` changes — a fresh error
//     mid-countdown doesn't get cut short by the previous one's
//     remaining clock.
//   * Renders a small × button that calls `onDismiss` immediately
//     so operators can clear stale errors without waiting.
//   * Pass `dismissAfterMs={0}` to opt out of auto-dismiss for
//     persistent / fatal banners (e.g. AppBoot's "control plane
//     unreachable" message that shouldn't disappear on its own).
//
// 2026-04-28 — added because long-running or transient 5xx errors
// stuck around forever, blocking the operator from seeing follow-up
// state. The 20s window is the documented contract; tweak via the
// per-callsite prop if the message warrants longer dwell time.

import { useEffect, type CSSProperties } from "react";

const DEFAULT_DISMISS_MS = 20_000;

interface Props {
    /** The error to display. `null` / `undefined` / `""` renders nothing. */
    message: string | null | undefined;
    /** Callback the parent uses to clear its own error state. Fired
     *  by the timer AND by the inline × button. */
    onDismiss: () => void;
    /** Tail of utility classes (margin, etc.) — mirrors what each
     *  callsite used to pass on the bare div. */
    className?: string;
    /** Override the auto-dismiss interval; pass `0` to disable
     *  (banner stays until `message` is reset by the parent or the
     *  user clicks the inline ×). */
    dismissAfterMs?: number;
    /** Test seam — exposed so callsite-specific tests can target
     *  this banner instance. */
    testId?: string;
}

const DISMISS_BUTTON_STYLE: CSSProperties = {
    background: "transparent",
    border: 0,
    color: "inherit",
    cursor: "pointer",
    fontSize: "1rem",
    lineHeight: 1,
    padding: "0 0.25rem",
    marginLeft: "0.5rem",
    opacity: 0.65,
};

const ROW_STYLE: CSSProperties = {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: "0.5rem",
};

export function ErrorBanner({
    message,
    onDismiss,
    className,
    dismissAfterMs = DEFAULT_DISMISS_MS,
    testId,
}: Props) {
    useEffect(() => {
        if (!message) return;
        if (dismissAfterMs <= 0) return;
        const id = window.setTimeout(() => {
            onDismiss();
        }, dismissAfterMs);
        return () => window.clearTimeout(id);
    }, [message, dismissAfterMs, onDismiss]);

    if (!message) return null;

    const cls =
        "execlaw-error-banner" + (className ? ` ${className}` : "");

    return (
        <div
            className={cls}
            role="alert"
            data-testid={testId ?? "error-banner"}
            style={ROW_STYLE}
        >
            <span>{message}</span>
            <button
                type="button"
                onClick={onDismiss}
                style={DISMISS_BUTTON_STYLE}
                aria-label="Dismiss error"
                data-testid="error-banner-dismiss"
            >
                ×
            </button>
        </div>
    );
}
