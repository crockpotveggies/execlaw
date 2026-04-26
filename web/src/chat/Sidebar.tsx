// Sidebar: brand + new-chat + nav (Routines / Contacts / plugin UI panels)
// + thread list with external-channel filter + bottom user affordance.
//
// Per the locked Phase-6 layout (MIGRATION_PLAN §6/§8.2): controller
// thread always shows pinned at top; an external-channel toggle
// hides non-controller-DM threads when the user wants to focus on
// personal chats; plugin-declared UI panels show under the "More"
// section.

import { useState, type ReactNode } from "react";
import { Link, NavLink } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import type { UiPanelSummary } from "../api/endpoints";
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
    /**
     * Plugin-declared UI panels rendered under "More". Empty array
     * when no plugins are installed; `null` while loading.
     */
    uiPanels?: UiPanelSummary[] | null;
}

export function Sidebar({ onNewThread, onSignOut, uiPanels }: SidebarProps) {
    const auth = useAuth();
    const threads = useChatState((s) => s.threads);
    const activeId = useChatState((s) => s.activeId);
    // Pending-approvals badge — when the cold-contact flow has any
    // open approvals waiting on the controller, surface a count in
    // the sidebar so the operator notices even without an active
    // thread that surfaces the inline ApprovalCard.
    const pendingApprovalCount = useChatState(
        (s) => Object.keys(s.pendingApprovals).length,
    );
    // Firing-alert badge — operational anomalies surfaced through
    // §10's alert pipeline. Polled by Chat.tsx every 60s and
    // refreshed on focus.
    const alertFiringCount = useChatState((s) => s.alertFiringCount);

    const [hideExternal, setHideExternal] = useState(false);
    const [moreExpanded, setMoreExpanded] = useState(false);

    const visibleThreads = threads.filter((t) => {
        // Always show pinned (Control thread).
        if (t.is_pinned) return true;
        // Always show the active one so it doesn't vanish on toggle.
        if (t.conversation_id === activeId) return true;
        if (!hideExternal) return true;
        // hideExternal=true → show only ControllerDM-shaped threads.
        return t.kind === "ControllerDM";
    });
    const externalCount = threads.filter(
        (t) => !t.is_pinned && t.kind !== "ControllerDM",
    ).length;

    const panels = uiPanels ?? [];

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
                <SidebarNavLink
                    to="/settings/routines"
                    icon="bi-clock-history"
                    label="Routines"
                    testId="sidebar-routines"
                />
                <SidebarNavLink
                    to="/settings/contacts"
                    icon="bi-person-lines-fill"
                    label="Contacts"
                    testId="sidebar-contacts"
                />
                {pendingApprovalCount > 0 && (
                    <SidebarNavLink
                        to="/chat"
                        icon="bi-shield-exclamation"
                        label="Approvals"
                        testId="sidebar-approvals"
                        badge={pendingApprovalCount}
                    />
                )}
                {alertFiringCount > 0 && (
                    <SidebarNavLink
                        to="/settings/alerts"
                        icon="bi-bell-fill"
                        label="Alerts"
                        testId="sidebar-alerts"
                        badge={alertFiringCount}
                    />
                )}
                <button
                    type="button"
                    className="execlaw-thread-item w-100"
                    onClick={() => setMoreExpanded((v) => !v)}
                    aria-expanded={moreExpanded}
                    data-testid="sidebar-more-toggle"
                >
                    <i
                        className={
                            "bi execlaw-muted execlaw-thread-item__icon " +
                            (moreExpanded ? "bi-chevron-down" : "bi-three-dots")
                        }
                        aria-hidden
                    />
                    <span className="execlaw-thread-item__name">More</span>
                    {panels.length > 0 && (
                        <span className="execlaw-muted small">{panels.length}</span>
                    )}
                </button>
                {moreExpanded && (
                    <div className="ps-3" data-testid="sidebar-more-panels">
                        {panels.length === 0 ? (
                            <div className="execlaw-muted small px-2 py-1">
                                No plugin panels installed.
                            </div>
                        ) : (
                            panels.map((p) => (
                                <Link
                                    key={p.mount}
                                    to={`/${p.mount}`}
                                    className="execlaw-thread-item"
                                    data-testid="sidebar-panel"
                                >
                                    <i
                                        className="bi bi-puzzle execlaw-muted execlaw-thread-item__icon"
                                        aria-hidden
                                    />
                                    <span className="execlaw-thread-item__name">
                                        {p.plugin_id}
                                    </span>
                                </Link>
                            ))
                        )}
                    </div>
                )}
            </nav>

            {externalCount > 0 && (
                <div
                    className="d-flex align-items-center gap-2 px-3 py-1 execlaw-muted small"
                    data-testid="sidebar-external-toggle-row"
                >
                    <span className="flex-grow-1">Threads</span>
                    <label className="d-flex align-items-center gap-1">
                        <input
                            type="checkbox"
                            checked={hideExternal}
                            onChange={(e) => setHideExternal(e.target.checked)}
                            data-testid="sidebar-hide-external"
                        />
                        <span>Hide external ({externalCount})</span>
                    </label>
                </div>
            )}

            <div className="execlaw-sidebar__threads" data-testid="sidebar-threads">
                {visibleThreads.length === 0 ? (
                    <div className="execlaw-muted small px-2 pt-2">
                        No threads yet. Start a new chat to begin.
                    </div>
                ) : (
                    visibleThreads.map((t) => {
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
                        ? auth.user.display_name
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

interface SidebarNavLinkProps {
    to: string;
    icon: string;
    label: string;
    testId?: string;
    /** Optional integer count rendered as a small accent-coloured pill. */
    badge?: number;
}

function SidebarNavLink({ to, icon, label, testId, badge }: SidebarNavLinkProps) {
    // NavLink applies the `is-active` className when its `to` matches
    // the current URL — so the same class hooks we use for thread
    // items light up for these top-level destinations too.
    return (
        <NavLink
            to={to}
            className={({ isActive }) =>
                "execlaw-thread-item" + (isActive ? " is-active" : "")
            }
            data-testid={testId}
        >
            <i
                className={`bi ${icon} execlaw-muted execlaw-thread-item__icon`}
                aria-hidden
            />
            <span className="execlaw-thread-item__name">{label}</span>
            {badge !== undefined && badge > 0 && (
                <span
                    className="execlaw-nav-badge"
                    aria-label={`${badge} pending`}
                    data-testid={testId ? `${testId}-badge` : undefined}
                >
                    {badge}
                </span>
            )}
        </NavLink>
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
