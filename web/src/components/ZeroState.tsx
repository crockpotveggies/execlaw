// Shared empty-state component.
//
// Centered icon + title + description + optional CTA. Used by every
// list / two-pane destination (Skills, Routines, Automations,
// Research, etc.) so first-run views read consistently across the
// app instead of each page hand-rolling its own "no items" copy.
//
// Layout matches the Skills "no active skills" reference: large
// icon, single-line title, narrow muted description, and a
// callsite-supplied action node (typically a `<button>` or `<Link>`)
// pinned below the body. Pages that don't have a meaningful action
// — e.g. Research, where research jobs originate from the chat
// thread — omit the `action` prop and the slot collapses.
//
// The wrapper takes `flex: 1 1 auto` so dropping it into a flex
// column container vertically centres it. Use `data-testid` to keep
// existing tests' empty-state assertions working through the
// refactor.

import type { ReactNode } from "react";

interface Props {
    /** Bootstrap-icons class name, e.g. `bi-stars`. Rendered with
     *  the standard zero-state size + colour. */
    icon: string;
    /** One-line headline. Plain text or rich content. */
    title: ReactNode;
    /** Supporting copy. Wrap multiple paragraphs in a fragment if
     *  you need more than one line; the column clamps to a narrow
     *  reading width so long copy still feels intentional. */
    description: ReactNode;
    /** Optional CTA. Pass a `<button>` or router `<Link>`; omit for
     *  surfaces where the natural "next step" lives elsewhere
     *  (chat-driven flows like Research). */
    action?: ReactNode;
    /** Test seam — preserved across the refactor so existing
     *  `getByTestId("routines-empty")` assertions still target this
     *  block. */
    testId?: string;
}

export function ZeroState({
    icon,
    title,
    description,
    action,
    testId,
}: Props) {
    return (
        <div className="execlaw-zero-state" data-testid={testId}>
            <i
                className={`bi ${icon} execlaw-zero-state__icon`}
                aria-hidden
            />
            <div className="execlaw-zero-state__title">{title}</div>
            <div className="execlaw-zero-state__body">{description}</div>
            {action && (
                <div className="execlaw-zero-state__action">{action}</div>
            )}
        </div>
    );
}
