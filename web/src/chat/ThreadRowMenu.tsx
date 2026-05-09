// Per-thread hover-revealed action menu (delete / rename / pin).
//
// 2026-04-28 — added so operators can prune dev-iteration thread
// debris without spelunking through the API. The 3-dot button is
// invisible until the row is hovered (CSS opacity transition driven
// by `.execlaw-thread-row:hover` on the parent); clicking it opens
// the popup anchored to the right edge of the row.
//
// Behaviours:
//   * **Delete** (red): hard-delete via DELETE /api/chats/:id. The
//     parent is responsible for re-fetching the thread list and
//     clearing the active id when the deleted thread was active.
//   * **Rename**: swaps the row's label for an inline `<input>` and
//     PATCHes display_name on Enter / blur. Esc cancels.
//   * **Pin** / **Unpin**: PATCH is_pinned. Sidebar list re-orders
//     pinned-first via the server's existing list_thread_summaries
//     ordering.
//
// Click-outside dismissal is handled with a single document-level
// pointerdown listener installed only while the menu is open. Esc
// also dismisses. Keeping it lightweight rather than pulling in a
// full popper/headlessui dep — one popup at a time, no flip logic
// needed.

import { useEffect, useRef, useState } from "react";

interface Props {
    isPinned: boolean;
    /// Called with `true` when the user requested rename (parent
    /// flips the row label into an input). The parent owns the
    /// editing state so the menu can close immediately after the
    /// click without losing the input focus.
    onStartRename: () => void;
    onTogglePin: () => void;
    onDelete: () => void;
}

export function ThreadRowMenu({
    isPinned,
    onStartRename,
    onTogglePin,
    onDelete,
}: Props) {
    const [open, setOpen] = useState(false);
    const ref = useRef<HTMLDivElement | null>(null);

    useEffect(() => {
        if (!open) return;
        const onDown = (e: PointerEvent) => {
            if (ref.current && !ref.current.contains(e.target as Node)) {
                setOpen(false);
            }
        };
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") setOpen(false);
        };
        document.addEventListener("pointerdown", onDown);
        document.addEventListener("keydown", onKey);
        return () => {
            document.removeEventListener("pointerdown", onDown);
            document.removeEventListener("keydown", onKey);
        };
    }, [open]);

    return (
        <div ref={ref} onClick={(e) => e.stopPropagation()}>
            <button
                type="button"
                className={
                    "execlaw-thread-row__menu-btn" + (open ? " is-open" : "")
                }
                aria-label="Thread actions"
                aria-haspopup="menu"
                aria-expanded={open}
                data-testid="thread-row-menu-btn"
                onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    setOpen((v) => !v);
                }}
            >
                <i className="bi bi-three-dots-vertical" aria-hidden />
            </button>
            {open && (
                <div
                    className="execlaw-thread-menu"
                    role="menu"
                    data-testid="thread-row-menu"
                >
                    <button
                        type="button"
                        className="execlaw-thread-menu__item"
                        role="menuitem"
                        data-testid="thread-row-menu-rename"
                        onClick={() => {
                            setOpen(false);
                            onStartRename();
                        }}
                    >
                        <i className="bi bi-pencil" aria-hidden />
                        Rename
                    </button>
                    <button
                        type="button"
                        className="execlaw-thread-menu__item"
                        role="menuitem"
                        data-testid="thread-row-menu-pin"
                        onClick={() => {
                            setOpen(false);
                            onTogglePin();
                        }}
                    >
                        <i
                            className={
                                "bi " +
                                (isPinned ? "bi-pin-angle" : "bi-pin-angle-fill")
                            }
                            aria-hidden
                        />
                        {isPinned ? "Unpin" : "Pin to top"}
                    </button>
                    <button
                        type="button"
                        className="execlaw-thread-menu__item is-danger"
                        role="menuitem"
                        data-testid="thread-row-menu-delete"
                        onClick={() => {
                            setOpen(false);
                            onDelete();
                        }}
                    >
                        <i className="bi bi-trash" aria-hidden />
                        Delete
                    </button>
                </div>
            )}
        </div>
    );
}
