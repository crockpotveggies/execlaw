// Sidebar: nav stubs + thread list + bottom user affordance.
//
// Today the nav stubs (Tasks, Contacts, More) are visual placeholders;
// each becomes a real route in Phase 6b/6c. The thread list is fully
// wired: list comes from the chat store, click sets the active thread.

import { type ReactNode } from "react";
import { Link } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { setActiveThread, useChatState } from "./store";

const CONTROLLER_THREAD_PREFIX = "controller-thread:";

interface SidebarProps {
    onNewThread: () => void;
    /**
     * Optional sign-out handler. Defaults to the AuthContext's
     * `signOut` directly; the chat route overrides this to choreograph
     * a fade-out animation before dropping auth state.
     */
    onSignOut?: () => void;
}

export function Sidebar({ onNewThread, onSignOut }: SidebarProps) {
    const auth = useAuth();
    const threads = useChatState((s) => s.threads);
    const activeId = useChatState((s) => s.activeId);

    return (
        <aside className="execlaw-sidebar">
            <div className="execlaw-sidebar__head">
                <h1 className="execlaw-brand h6 mb-2">execlaw</h1>
                <button
                    type="button"
                    className="btn btn-primary btn-sm w-100 d-flex align-items-center justify-content-center gap-2"
                    onClick={onNewThread}
                    data-testid="sidebar-new-thread"
                >
                    <i className="bi bi-pencil-square" aria-hidden />
                    New chat
                </button>
            </div>

            <nav className="execlaw-sidebar__nav">
                <NavStub icon="bi-clipboard-check" label="Tasks" />
                <NavStub icon="bi-people" label="Contacts" />
                <NavStub icon="bi-three-dots" label="More" />
            </nav>

            <div className="execlaw-sidebar__threads" data-testid="sidebar-threads">
                {threads.length === 0 ? (
                    <div className="execlaw-muted small px-2 pt-2">
                        No threads yet. Start a new chat to begin.
                    </div>
                ) : (
                    threads.map((t) => {
                        const isControl = t.conversation_id.startsWith(
                            CONTROLLER_THREAD_PREFIX,
                        );
                        const label =
                            t.display_name ??
                            (isControl
                                ? "Control thread"
                                : `New chat · ${t.conversation_id.slice(0, 6)}`);
                        return (
                            <button
                                type="button"
                                key={t.conversation_id}
                                className={
                                    "execlaw-thread-item" +
                                    (t.conversation_id === activeId
                                        ? " is-active"
                                        : "")
                                }
                                onClick={() =>
                                    setActiveThread(t.conversation_id)
                                }
                                data-testid="sidebar-thread"
                                data-thread-id={t.conversation_id}
                            >
                                <ThreadStatusIcon
                                    isThinking={t.is_thinking}
                                    isUnread={t.has_unread}
                                    isPinned={t.is_pinned}
                                />
                                <span className="execlaw-thread-item__name">
                                    {label}
                                </span>
                                {t.is_ephemeral && (
                                    <i
                                        className="bi bi-incognito execlaw-muted"
                                        aria-label="Incognito thread"
                                    />
                                )}
                            </button>
                        );
                    })
                )}
            </div>

            <div className="execlaw-sidebar__foot">
                <Link
                    to="/settings"
                    className="btn btn-link btn-sm p-0 execlaw-muted"
                    data-testid="sidebar-settings"
                    aria-label="Settings"
                >
                    <i className="bi bi-gear" aria-hidden />
                </Link>
                <span className="execlaw-thread-item__name">
                    {auth.user
                        ? `${auth.user.display_name} @${auth.user.username}`
                        : "—"}
                </span>
                <button
                    type="button"
                    className="btn btn-link btn-sm p-0 ms-auto execlaw-muted"
                    onClick={onSignOut ?? auth.signOut}
                    data-testid="sidebar-signout"
                    aria-label="Sign out"
                >
                    <i className="bi bi-box-arrow-right" aria-hidden />
                </button>
            </div>
        </aside>
    );
}

function NavStub({ icon, label }: { icon: string; label: string }) {
    return (
        <div className="execlaw-thread-item" aria-disabled="true">
            <i
                className={`bi ${icon} execlaw-muted execlaw-thread-item__icon`}
                aria-hidden
            />
            <span className="execlaw-thread-item__name">{label}</span>
        </div>
    );
}

interface IconProps {
    isThinking: boolean;
    isUnread: boolean;
    isPinned: boolean;
}

function ThreadStatusIcon({ isThinking, isUnread, isPinned }: IconProps): ReactNode {
    if (isThinking) {
        return (
            <span
                className="execlaw-thread-item__icon"
                aria-label="Agent processing"
            >
                <span className="execlaw-thread-spinner" />
            </span>
        );
    }
    if (isPinned) {
        return (
            <span className="execlaw-thread-item__icon" aria-label="Pinned">
                <i className="bi bi-pin-angle-fill" aria-hidden />
            </span>
        );
    }
    return (
        <span
            className="execlaw-thread-item__icon"
            aria-label={isUnread ? "Unread" : "Read"}
        >
            <span
                className={
                    "execlaw-thread-dot" + (isUnread ? " is-unread" : "")
                }
            />
        </span>
    );
}
