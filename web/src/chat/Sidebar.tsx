// Sidebar: brand + new-chat + nav (Routines / Contacts / plugin UI panels)
// + thread list with external-channel filter + bottom user affordance.
//
// Per the locked Phase-6 layout (MIGRATION_PLAN §6/§8.2): controller
// thread always shows pinned at top; an external-channel toggle
// hides non-controller-DM threads when the user wants to focus on
// personal chats; plugin-declared UI panels show under the "More"
// section.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link, NavLink, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import {
    deleteThread,
    listThreads,
    patchThread,
    type UiPanelSummary,
} from "../api/endpoints";
import { setActiveThread, setThreads, useChatState } from "./store";
import { ThreadRowMenu } from "./ThreadRowMenu";

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
    const navigate = useNavigate();
    const threads = useChatState((s) => s.threads);
    const activeId = useChatState((s) => s.activeId);
    // 2026-04-28 — inline-rename state. When the user clicks
    // "Rename" in a thread's hover menu, we stash that thread's id
    // here; the row swaps its label for an `<input>` until the user
    // commits (Enter / blur) or cancels (Esc).
    const [renamingId, setRenamingId] = useState<string | null>(null);
    const getToken = auth.getAccessToken;
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
                <h1 className="execlaw-brand h6 mb-0">execlaw</h1>
            </div>

            <nav className="execlaw-sidebar__nav">
                {/*
                  "New chat" intentionally renders as a plain text
                  row (same visual class as Routines / Contacts)
                  rather than a filled button. Lives inside
                  `__nav` rather than `__head` so it inherits the
                  same horizontal padding as the other nav items —
                  otherwise the icon column is offset and the row
                  breaks the vertical-list rhythm.
                */}
                <button
                    type="button"
                    className="execlaw-thread-item"
                    onClick={onNewThread}
                    data-testid="sidebar-new-thread"
                >
                    <i
                        className="bi bi-pencil-square execlaw-muted execlaw-thread-item__icon"
                        aria-hidden
                    />
                    <span className="execlaw-thread-item__name">
                        New chat
                    </span>
                </button>
                <div className="execlaw-sidebar__section">Browse</div>
                <SidebarNavLink
                    to="/routines"
                    icon="bi-clock-history"
                    label="Routines"
                    testId="sidebar-routines"
                />
                <SidebarNavLink
                    to="/research"
                    icon="bi-binoculars"
                    label="Research"
                    testId="sidebar-research"
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
                <div className="execlaw-sidebar__section">Threads</div>
                {visibleThreads.length === 0 ? (
                    <div className="execlaw-muted small px-2 pt-2">
                        No threads yet. Start a new chat to begin.
                    </div>
                ) : (
                    visibleThreads.map((t) => {
                        const isControl = t.conversation_id.startsWith(
                            CONTROLLER_THREAD_PREFIX,
                        );
                        const fallback = isControl
                            ? "Control thread"
                            : `New chat · ${t.conversation_id.slice(0, 6)}`;
                        const label = t.display_name ?? fallback;
                        const isRenaming = renamingId === t.conversation_id;
                        return (
                            <ThreadRow
                                key={t.conversation_id}
                                conversationId={t.conversation_id}
                                label={label}
                                fallbackLabel={fallback}
                                isActive={t.conversation_id === activeId}
                                isProcessing={t.is_processing}
                                hasUnread={t.has_unread}
                                isPinned={t.is_pinned}
                                isEphemeral={t.is_ephemeral}
                                isRenaming={isRenaming}
                                onActivate={() => {
                                    setActiveThread(t.conversation_id);
                                    navigate(
                                        `/chat/${encodeURIComponent(
                                            t.conversation_id,
                                        )}`,
                                    );
                                }}
                                onStartRename={() =>
                                    setRenamingId(t.conversation_id)
                                }
                                onCommitRename={async (next) => {
                                    setRenamingId(null);
                                    const trimmed = next.trim();
                                    // No-op if unchanged; PATCH with
                                    // null when cleared so the row
                                    // falls back to the auto-label.
                                    const send: string | null =
                                        trimmed.length === 0 ? null : trimmed;
                                    if ((t.display_name ?? null) === send)
                                        return;
                                    try {
                                        await patchThread(
                                            t.conversation_id,
                                            { display_name: send },
                                            getToken,
                                        );
                                        const r = await listThreads(getToken);
                                        setThreads(r.threads);
                                    } catch (e) {
                                        console.warn("rename failed", e);
                                    }
                                }}
                                onCancelRename={() => setRenamingId(null)}
                                onTogglePin={async () => {
                                    try {
                                        await patchThread(
                                            t.conversation_id,
                                            { is_pinned: !t.is_pinned },
                                            getToken,
                                        );
                                        const r = await listThreads(getToken);
                                        setThreads(r.threads);
                                    } catch (e) {
                                        console.warn("pin toggle failed", e);
                                    }
                                }}
                                onDelete={async () => {
                                    // Defensive confirm — it's a hard
                                    // delete with no undo. The server
                                    // treats the call as idempotent so
                                    // a Cancel is the only way out
                                    // here.
                                    if (
                                        !window.confirm(
                                            `Delete "${label}"? This wipes the conversation's history.`,
                                        )
                                    ) {
                                        return;
                                    }
                                    try {
                                        await deleteThread(
                                            t.conversation_id,
                                            getToken,
                                        );
                                        // Drop active id if we just
                                        // deleted the active thread —
                                        // otherwise the chat pane
                                        // briefly flashes "no
                                        // messages yet" before the
                                        // list refresh clears it.
                                        if (activeId === t.conversation_id) {
                                            setActiveThread(null);
                                            navigate("/chat");
                                        }
                                        const r = await listThreads(getToken);
                                        setThreads(r.threads);
                                    } catch (e) {
                                        console.warn("delete failed", e);
                                    }
                                }}
                            />
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

interface ThreadRowProps {
    conversationId: string;
    label: string;
    fallbackLabel: string;
    isActive: boolean;
    isProcessing: boolean;
    hasUnread: boolean;
    isPinned: boolean;
    isEphemeral: boolean;
    isRenaming: boolean;
    onActivate: () => void;
    onStartRename: () => void;
    onCommitRename: (next: string) => void;
    onCancelRename: () => void;
    onTogglePin: () => void;
    onDelete: () => void;
}

function ThreadRow({
    conversationId,
    label,
    isActive,
    isProcessing,
    hasUnread,
    isPinned,
    isEphemeral,
    isRenaming,
    onActivate,
    onStartRename,
    onCommitRename,
    onCancelRename,
    onTogglePin,
    onDelete,
}: ThreadRowProps) {
    // Wrapping the row in a `<div>` gives us a stable hover target
    // for the 3-dot button reveal. Keeping the inner click handler
    // on the body so the whole pill (minus the menu button) still
    // selects the thread on click — same UX as the previous plain
    // button.
    const inputRef = useRef<HTMLInputElement | null>(null);
    const [draft, setDraft] = useState(label);

    useEffect(() => {
        if (isRenaming) {
            setDraft(label);
            // Defer focus to next tick so React has actually swapped
            // the label-span out for the input element before we
            // call .focus()/.select().
            queueMicrotask(() => {
                inputRef.current?.focus();
                inputRef.current?.select();
            });
        }
    }, [isRenaming, label]);

    return (
        <div
            className={
                "execlaw-thread-row execlaw-thread-item" +
                (isActive ? " is-active" : "")
            }
            onClick={onActivate}
            role="button"
            tabIndex={0}
            onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onActivate();
                }
            }}
            data-testid="sidebar-thread"
            data-thread-id={conversationId}
        >
            <ThreadStatusIcon
                isProcessing={isProcessing}
                isUnread={hasUnread}
                isPinned={isPinned}
            />
            {isRenaming ? (
                <input
                    ref={inputRef}
                    className="execlaw-thread-rename-input"
                    value={draft}
                    onChange={(e) => setDraft(e.target.value)}
                    onClick={(e) => e.stopPropagation()}
                    onBlur={() => onCommitRename(draft)}
                    onKeyDown={(e) => {
                        e.stopPropagation();
                        if (e.key === "Enter") {
                            e.preventDefault();
                            onCommitRename(draft);
                        } else if (e.key === "Escape") {
                            e.preventDefault();
                            onCancelRename();
                        }
                    }}
                    data-testid="sidebar-thread-rename-input"
                />
            ) : (
                <span className="execlaw-thread-item__name">{label}</span>
            )}
            {isEphemeral && (
                <i
                    className="bi bi-incognito execlaw-muted"
                    aria-label="Incognito thread"
                />
            )}
            <ThreadRowMenu
                isPinned={isPinned}
                onStartRename={onStartRename}
                onTogglePin={onTogglePin}
                onDelete={onDelete}
            />
        </div>
    );
}

interface IconProps {
    isProcessing: boolean;
    isUnread: boolean;
    isPinned: boolean;
}

function ThreadStatusIcon({ isProcessing, isUnread, isPinned }: IconProps): ReactNode {
    if (isProcessing) {
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
