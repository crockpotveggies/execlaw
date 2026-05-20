// Sidebar: brand + new-chat + nav (Routines / Contacts / plugin UI panels)
// + thread list with external-channel filter + bottom user affordance.
//
// Per the locked Phase-6 layout (MIGRATION_PLAN §6/§8.2): controller
// thread always shows pinned at top; an external-channel toggle
// hides non-controller-DM threads when the user wants to focus on
// personal chats; plugin-declared UI panels show under the "More"
// section.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link, NavLink, useLocation, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { useT } from "../i18n";
import {
    deleteThread,
    getAlertCount,
    listPendingApprovals,
    listThreads,
    patchThread,
    type UiPanelSummary,
} from "../api/endpoints";
import { useConnectionStatus } from "../api/connection";
import { useAnyBackendInstalling } from "./useAnyBackendInstalling";
import {
    setActiveThread,
    setAlertFiringCount,
    setPendingApprovals,
    setThreads,
    useChatState,
} from "./store";
import { ChannelIcon } from "../components/ChannelIcons";
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
    const tr = useT();
    const navigate = useNavigate();
    const location = useLocation();
    const threads = useChatState((s) => s.threads);
    const activeIdRaw = useChatState((s) => s.activeId);
    // 2026-05-04 — gate the thread `.is-active` highlight on the
    // operator actually being on a /chat route. The chat store keeps
    // `activeId` set across navigation so coming back to /chat re-
    // surfaces the last viewed thread, but the sidebar shouldn't
    // render that thread as "current page" while the operator is on
    // /research, /settings, /routines, etc. Without this gate the
    // highlighted row read as "you're viewing this thread right
    // now," which is wrong on every non-chat route.
    const isOnChatRoute = location.pathname.startsWith("/chat");
    const activeId = isOnChatRoute ? activeIdRaw : null;
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
    // §10's alert pipeline. Loaded + polled by the Sidebar mount
    // effect below so the badge tracks alerts on every route, not
    // just /chat.
    const alertFiringCount = useChatState((s) => s.alertFiringCount);

    // Phase 6.5 (Apple Silicon plan) — brand indicator's "installing"
    // state. Hook polls every backend purpose's `/status` and returns
    // true while any one is pulling / starting / loading. The brand
    // indicator's render branch picks `is-installing` when this is
    // true AND there are no active alerts (alert wins; see precedence
    // logic inside the component).
    const anyBackendInstalling = useAnyBackendInstalling(
        getToken,
        auth.status === "authenticated",
    );

    const [hideExternal, setHideExternal] = useState(false);
    const [moreExpanded, setMoreExpanded] = useState(false);
    const [filtersOpen, setFiltersOpen] = useState(false);
    // Click-outside handler for the Threads → Filters dropdown.
    // Closes the menu when the operator clicks anywhere else,
    // matching browser-native dropdown behaviour.
    const filtersRef = useRef<HTMLDivElement | null>(null);
    useEffect(() => {
        if (!filtersOpen) return;
        const onClick = (e: MouseEvent) => {
            if (
                filtersRef.current &&
                !filtersRef.current.contains(e.target as Node)
            ) {
                setFiltersOpen(false);
            }
        };
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") setFiltersOpen(false);
        };
        document.addEventListener("mousedown", onClick);
        document.addEventListener("keydown", onKey);
        return () => {
            document.removeEventListener("mousedown", onClick);
            document.removeEventListener("keydown", onKey);
        };
    }, [filtersOpen]);

    // 2026-05-04 — Sidebar owns the load of every piece of state it
    // renders: thread list, pending-approval count, firing-alert
    // count. Used to live in Chat.tsx, which meant a refresh on a
    // non-chat route (/settings, /routines, /research, /skills) left
    // the sidebar's thread list permanently empty. Hoisting the
    // fetch here covers every route that mounts a Sidebar.
    //
    // Refetches on each Sidebar mount; navigation between routes
    // unmounts/remounts each route's Sidebar instance, which gives
    // the operator a fresh thread list whenever they come back to
    // any chrome that includes the sidebar. The three calls fire in
    // parallel — total wall-clock is one round-trip.
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        let cancelled = false;
        (async () => {
            try {
                const [threadsResp, approvalsResp, alertCount] =
                    await Promise.all([
                        listThreads(getToken),
                        listPendingApprovals(getToken),
                        getAlertCount(getToken).catch(() => ({
                            firing_count: 0,
                        })),
                    ]);
                if (cancelled) return;
                setThreads(threadsResp.threads);
                setPendingApprovals(approvalsResp.approvals);
                setAlertFiringCount(alertCount.firing_count);
            } catch {
                // Silent — a transient sidebar-load failure shouldn't
                // pollute the route the operator is actually on.
                // ConnectionBanner already covers the "server is
                // unreachable" case.
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [auth.status, getToken]);

    // Cheap firing-count poll every 60s so the badge tracks alerts
    // that arrive while the operator is sitting on any sidebar-
    // bearing route. Switch to WS-pushed alert events once the alert
    // bus lands (§10.8).
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        const id = window.setInterval(async () => {
            try {
                const r = await getAlertCount(getToken);
                setAlertFiringCount(r.firing_count);
            } catch {
                // Silent — transient failures shouldn't pollute the UI.
            }
        }, 60_000);
        return () => window.clearInterval(id);
    }, [auth.status, getToken]);

    const visibleThreads = threads.filter((t) => {
        // Always show pinned (Control thread).
        if (t.is_pinned) return true;
        // Always show the active one so it doesn't vanish on toggle.
        // Use the RAW active id (not the route-gated one) so the
        // last-viewed thread stays visible while the operator is on
        // Settings / Research / Routines — switching back to it is
        // one click. Only the .is-active CSS class respects the
        // current route.
        if (t.conversation_id === activeIdRaw) return true;
        if (!hideExternal) return true;
        // hideExternal=true → hide threads bridged onto a non-web
        // transport. Source of truth is `transport_channel` from the
        // server (presence of a `state_transport_bindings` row), NOT
        // the conversation `kind` — the inbound path stamps `kind`
        // generically (see chats::ensure_conversation), so kind-based
        // filtering left Signal groups visible when the toggle was
        // off (the regression that prompted this rewrite).
        return !t.transport_channel;
    });

    // 2026-05-05 — `uiPanels` is still threaded through the prop
    // for compat with callers (Chat / Settings still fetch the
    // list) but we no longer render panel entries in the sidebar.
    // See note inside the More section for why.
    void uiPanels;

    return (
        <aside className="execlaw-sidebar">
            <div className="execlaw-sidebar__head">
                <h1 className="execlaw-brand h6 mb-0">execlaw</h1>
                <BrandStatusIndicator
                    alertCount={alertFiringCount}
                    installing={anyBackendInstalling}
                />
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
                        {tr("sidebar.newChat", "New chat")}
                    </span>
                </button>
                <div className="execlaw-sidebar__section">
                    {tr("sidebar.browse", "Browse")}
                </div>
                <SidebarNavLink
                    to="/routines"
                    icon="bi-clock-history"
                    label={tr("sidebar.routines", "Routines")}
                    testId="sidebar-routines"
                />
                <SidebarNavLink
                    to="/automations"
                    icon="bi-lightning-charge-fill"
                    label={tr("sidebar.automations", "Automations")}
                    testId="sidebar-automations"
                />
                <SidebarNavLink
                    to="/research"
                    icon="bi-binoculars"
                    label={tr("sidebar.research", "Research")}
                    testId="sidebar-research"
                />
                <SidebarNavLink
                    to="/skills"
                    icon="bi-stars"
                    label={tr("sidebar.skills", "Skills")}
                    testId="sidebar-skills"
                />
                {pendingApprovalCount > 0 && (
                    <SidebarNavLink
                        to="/approvals"
                        icon="bi-shield-exclamation"
                        label={tr("sidebar.approvals", "Approvals")}
                        testId="sidebar-approvals"
                        badge={pendingApprovalCount}
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
                    <span className="execlaw-thread-item__name">
                        {tr("sidebar.more", "More")}
                    </span>
                </button>
                {moreExpanded && (
                    <div className="ps-3" data-testid="sidebar-more-panels">
                        <SidebarNavLink
                            to="/settings/contacts"
                            icon="bi-person-lines-fill"
                            label={tr("sidebar.contacts", "Contacts")}
                            testId="sidebar-contacts"
                        />
                        {/*
                          2026-05-05 — plugin UI panels were briefly
                          rendered here, which broke the established
                          "plugins are configured via the gear icon
                          on Settings → Plugins" pattern. Each
                          plugin's `[[ui_panels]]` declaration now
                          surfaces only as a `has_settings_ui = true`
                          flag on the plugins-list row, gating the
                          gear icon next to the plugin's toggle.
                          Sidebar panel cluttering retired.
                        */}
                    </div>
                )}
            </nav>

            <div className="execlaw-sidebar__threads" data-testid="sidebar-threads">
                <div
                    className="execlaw-sidebar__section d-flex align-items-center"
                    style={{ position: "relative" }}
                    ref={filtersRef}
                >
                    <span className="flex-grow-1">
                        {tr("sidebar.threads", "Threads")}
                    </span>
                    <button
                        type="button"
                        className="btn btn-link btn-sm p-0 execlaw-muted"
                        onClick={() => setFiltersOpen((v) => !v)}
                        aria-haspopup="menu"
                        aria-expanded={filtersOpen}
                        data-testid="sidebar-threads-filters"
                        title={tr("sidebar.filters", "Filters")}
                        style={{
                            lineHeight: 1,
                            display: "inline-flex",
                            alignItems: "center",
                        }}
                    >
                        <i className="bi bi-sliders" aria-hidden />
                    </button>
                    {filtersOpen && (
                        <div
                            role="menu"
                            className="execlaw-card"
                            data-testid="sidebar-threads-filters-menu"
                            style={{
                                position: "absolute",
                                top: "100%",
                                right: 0,
                                zIndex: 100,
                                minWidth: "13rem",
                                marginTop: "0.25rem",
                                padding: "0.25rem",
                                background: "#1f2630",
                                border: "1px solid #30363d",
                                boxShadow: "0 8px 24px rgba(0,0,0,0.45)",
                                // Reset the section-header text
                                // styling we inherited from the
                                // wrapping `.execlaw-sidebar__section`
                                // div (uppercase, 0.6rem, muted).
                                // The menu is a full popover — its
                                // contents should read as regular
                                // page text, not as a sub-heading
                                // of the heading that hosts it.
                                textTransform: "none",
                                letterSpacing: "normal",
                                fontSize: "0.875rem",
                                fontWeight: 400,
                                color: "#e6edf3",
                            }}
                        >
                            <button
                                type="button"
                                onClick={() =>
                                    setHideExternal((v) => !v)
                                }
                                data-testid="sidebar-hide-external"
                                aria-pressed={hideExternal}
                                className="d-flex align-items-center w-100"
                                style={{
                                    background: "transparent",
                                    border: "none",
                                    color: "inherit",
                                    padding: "0.4rem 0.6rem",
                                    borderRadius: "0.25rem",
                                    cursor: "pointer",
                                    textAlign: "left",
                                }}
                            >
                                <span className="flex-grow-1">
                                    {tr(
                                        "sidebar.externalChannels",
                                        "External channels",
                                    )}
                                </span>
                                <span
                                    data-testid="sidebar-hide-external-state"
                                    style={{
                                        fontWeight: 600,
                                        color: hideExternal
                                            ? "#7d8590"
                                            : "#3fb950",
                                        marginLeft: "0.5rem",
                                    }}
                                >
                                    {hideExternal
                                        ? tr("sidebar.off", "off")
                                        : tr("sidebar.on", "on")}
                                </span>
                            </button>
                        </div>
                    )}
                </div>
                {visibleThreads.length === 0 ? (
                    <div className="execlaw-muted small px-2 pt-2">
                        {tr(
                            "sidebar.noThreads",
                            "No threads yet. Start a new chat to begin.",
                        )}
                    </div>
                ) : (
                    visibleThreads.map((t) => {
                        const isControl = t.conversation_id.startsWith(
                            CONTROLLER_THREAD_PREFIX,
                        );
                        const fallback = isControl
                            ? tr("sidebar.controlThread", "Control thread")
                            : tr(
                                  "sidebar.newChatLabel",
                                  "New chat · {{id}}",
                                  { id: t.conversation_id.slice(0, 6) },
                              );
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
                                transportChannel={t.transport_channel ?? null}
                                transportIcon={t.transport_icon ?? null}
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
                                            tr(
                                                "sidebar.deleteConfirm",
                                                'Delete "{{name}}"? This wipes the conversation\'s history.',
                                                { name: label },
                                            ),
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
                                        // Use the RAW active id (not
                                        // route-gated): if the
                                        // operator's last-viewed
                                        // thread was this one, we
                                        // still need to drop it
                                        // even when deleting from a
                                        // non-chat route.
                                        if (activeIdRaw === t.conversation_id) {
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
                    aria-label={tr("sidebar.settings", "Settings")}
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
                    aria-label={tr("sidebar.signOut", "Sign out")}
                >
                    <i className="bi bi-box-arrow-right" aria-hidden />
                </button>
            </div>
        </aside>
    );
}

// Inline status indicator that lives next to the brand wordmark.
// Three states, in priority order:
//   * disconnected (server unreachable / WS reconnecting) — wifi-off
//     icon. Wins over alerts because if the SPA can't reach the
//     control plane, the alert count it last cached is stale.
//   * firing alerts > 0 — alert-triangle icon. Click to jump to
//     /settings/alerts; this is the replacement for the conditional
//     "Alerts" nav row that used to appear only when alerts were
//     active.
//   * healthy — small green dot. Always visible so the operator has
//     a steady "everything is fine" signal in the same spot.
function BrandStatusIndicator({
    alertCount,
    installing,
}: {
    alertCount: number;
    /// True while any managed backend is in a Pulling/Starting/loading
    /// phase. Drives the `is-installing` variant (pulsing download
    /// icon → /settings/backends). Computed in the parent so the
    /// poll cost is paid once per Sidebar mount, not per render.
    installing: boolean;
}) {
    const tr = useT();
    const conn = useConnectionStatus();
    // Precedence (highest → lowest): disconnected > alert > installing > ok.
    // - Connection loss is the most urgent signal — without it nothing
    //   else is meaningfully observable, so it wins over alerts that
    //   may be stale.
    // - Alerts beat installing because a firing alert means something
    //   is broken; an install in flight is just slow, not broken.
    // - Installing beats ok so the operator notices the install is
    //   still running and can click through to /settings/backends.
    if (conn !== "online") {
        const label =
            conn === "offline"
                ? tr("sidebar.status.offline", "Server unreachable")
                : tr("sidebar.status.reconnecting", "Reconnecting to server");
        return (
            <span
                className="execlaw-brand-status is-disconnected"
                role="status"
                aria-label={label}
                title={label}
                data-testid="sidebar-brand-status"
                data-state="disconnected"
            >
                <i className="bi bi-wifi-off" aria-hidden />
            </span>
        );
    }
    if (alertCount > 0) {
        const label =
            alertCount === 1
                ? tr(
                      "sidebar.status.alertSingular",
                      "{{count}} firing alert",
                      { count: alertCount },
                  )
                : tr(
                      "sidebar.status.alertPlural",
                      "{{count}} firing alerts",
                      { count: alertCount },
                  );
        return (
            <Link
                to="/settings/alerts"
                className="execlaw-brand-status is-alert"
                aria-label={label}
                title={label}
                data-testid="sidebar-brand-status"
                data-state="alert"
            >
                <i className="bi bi-exclamation-triangle-fill" aria-hidden />
            </Link>
        );
    }
    if (installing) {
        const label = tr(
            "sidebar.status.installing",
            "Backend installing — open Backends page",
        );
        return (
            <Link
                to="/settings/backends"
                className="execlaw-brand-status is-installing"
                aria-label={label}
                title={label}
                data-testid="sidebar-brand-status"
                data-state="installing"
            >
                <i className="bi bi-cloud-download-fill" aria-hidden />
            </Link>
        );
    }
    return (
        <Link
            to="/settings/alerts"
            className="execlaw-brand-status is-ok"
            aria-label={tr("sidebar.status.online", "Online — open alerts")}
            title={tr("sidebar.status.online", "Online — open alerts")}
            data-testid="sidebar-brand-status"
            data-state="ok"
        />
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
    /// Channel name (e.g. "signal") for bridged threads, or `null`
    /// for web-only chats. Drives the per-row marker icon next to
    /// the label so the operator can spot Signal threads at a
    /// glance.
    transportChannel: string | null;
    /// Bootstrap-icons name (sans `bi-` prefix) supplied by the
    /// transport plugin's manifest, or `null` for web-only chats.
    transportIcon: string | null;
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
    transportChannel,
    transportIcon,
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
            {transportChannel && (
                // 2026-05-12 — ChannelIcon owns the brand-vs-bi-*
                // resolution chain (signal → official SignalLogo
                // SVG; whatsapp/discord/slack/telegram → bi-<name>
                // brand glyphs; manifest icon override → bi-<icon>;
                // default fallback → bi-chat-quote). Pre-rework
                // every transport row rendered `bi-${manifest.icon}`
                // with no brand-SVG fallback, so a Signal thread
                // showed bi-chat-quote (Signal's manifest icon)
                // instead of the actual Signal logo. The component
                // also supplies a default when the manifest omits
                // the field, so future plugins don't need to
                // declare an icon for the sidebar to look correct.
                <span
                    className="execlaw-muted"
                    title={`Bridged on ${transportChannel}`}
                    style={{ marginLeft: "0.25rem", display: "inline-flex" }}
                >
                    <ChannelIcon
                        channel={transportChannel}
                        manifestIcon={transportIcon}
                        monochrome
                        decorative
                        data-testid="thread-channel-icon"
                    />
                </span>
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
