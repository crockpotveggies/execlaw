// Settings shell — sidebar tabs + active sub-page.
//
// Routes nested under /settings/* render a single right-hand pane
// inside the chat shell so the sidebar (threads + new chat) is still
// reachable from anywhere. Drop the active thread when entering
// settings so the welcome view doesn't flash.

import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { useScreenTransition } from "../anim/useScreenTransition";
import { useCallback, useEffect, useState } from "react";
import { setActiveThread } from "../chat/store";
import { useNavigate } from "react-router-dom";
import { listUiPanels, type UiPanelSummary } from "../api/endpoints";

import { PluginsPage } from "./PluginsPage";
import { LogsPage } from "./LogsPage";
import { EvalFlagsPage } from "./EvalFlagsPage";
import { PrincipalsPage } from "./PrincipalsPage";
import { AuditPage } from "./AuditPage";
import { BackendsPage } from "./BackendsPage";
import { UserPage } from "./UserPage";
import { ToolsPage } from "./ToolsPage";
import { McpServersPage } from "./McpServersPage";
import { RunnersPage } from "./RunnersPage";

const TABS: ReadonlyArray<{ to: string; icon: string; label: string }> = [
    // User first — operator accounts, password, passkeys, sessions.
    // Singular "User" because it's the page about *your* account
    // (other operators are listed inside it for Controllers). Was
    // briefly called "Login" but that read as the sign-in screen.
    { to: "/settings/user", icon: "bi-person-circle", label: "User" },
    { to: "/settings/plugins", icon: "bi-plug", label: "Plugins" },
    { to: "/settings/tools", icon: "bi-wrench-adjustable", label: "Tools" },
    { to: "/settings/mcp", icon: "bi-broadcast", label: "MCP" },
    // Hardware now lives at the bottom of the Backends page.
    { to: "/settings/backends", icon: "bi-cpu-fill", label: "Backends" },
    { to: "/settings/runners", icon: "bi-fire", label: "Runners" },
    { to: "/settings/principals", icon: "bi-people", label: "Principals" },
    { to: "/settings/logs", icon: "bi-list-columns", label: "Logs" },
    { to: "/settings/eval", icon: "bi-bar-chart", label: "Eval flags" },
    { to: "/settings/audit", icon: "bi-journal-text", label: "Audit" },
];

export function Settings() {
    const auth = useAuth();
    const navigate = useNavigate();
    const { ref: shellRef, dismiss } = useScreenTransition<HTMLDivElement>({
        initialScale: 1,
        exitScale: 1,
        durationMs: 220,
    });
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [uiPanels, setUiPanels] = useState<UiPanelSummary[] | null>(null);
    useEffect(() => {
        if (auth.status !== "authenticated") return;
        let cancelled = false;
        (async () => {
            try {
                const r = await listUiPanels(getToken);
                if (!cancelled) setUiPanels(r.panels);
            } catch {
                if (!cancelled) setUiPanels([]);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [auth.status, getToken]);

    const handleSignOut = useCallback(() => {
        dismiss(() => {
            void auth.signOut();
        });
    }, [auth, dismiss]);

    const onNewThread = useCallback(() => {
        // Drop active-thread state, then route to /chat — the chat
        // route picks up with a fresh welcome view.
        setActiveThread(null);
        navigate("/chat");
    }, [navigate]);

    if (auth.status === "loading") {
        return (
            <div className="execlaw-auth-shell">
                <div className="execlaw-muted small">Loading session…</div>
            </div>
        );
    }
    if (auth.status === "unauthenticated") {
        return <Navigate to="/login" replace />;
    }

    return (
        <div ref={shellRef} className="execlaw-shell">
            <Sidebar
                onNewThread={onNewThread}
                onSignOut={handleSignOut}
                uiPanels={uiPanels}
            />
            <main className="execlaw-main">
                <header className="execlaw-main__head">
                    <h2 className="h6 mb-0">
                        <i className="bi bi-gear me-2" aria-hidden />
                        Settings
                    </h2>
                </header>
                <div className="execlaw-settings">
                    <nav
                        className="execlaw-settings__tabs"
                        aria-label="Settings sections"
                    >
                        {TABS.map((t) => (
                            <NavLink
                                key={t.to}
                                to={t.to}
                                className={({ isActive }) =>
                                    "execlaw-settings__tab" +
                                    (isActive ? " is-active" : "")
                                }
                            >
                                <i className={`bi ${t.icon} me-2`} aria-hidden />
                                {t.label}
                            </NavLink>
                        ))}
                    </nav>

                    <Routes>
                        <Route
                            index
                            element={<Navigate to="user" replace />}
                        />
                        <Route path="user" element={<UserPage />} />
                        <Route path="plugins" element={<PluginsPage />} />
                        <Route path="tools" element={<ToolsPage />} />
                        <Route path="mcp" element={<McpServersPage />} />
                        <Route path="backends" element={<BackendsPage />} />
                        <Route path="runners" element={<RunnersPage />} />
                        <Route path="principals" element={<PrincipalsPage />} />
                        <Route path="logs" element={<LogsPage />} />
                        <Route path="eval" element={<EvalFlagsPage />} />
                        <Route path="audit" element={<AuditPage />} />
                        {/* Legacy paths fall back to /settings/user. */}
                        <Route
                            path="login"
                            element={<Navigate to="../user" replace />}
                        />
                        <Route
                            path="profile"
                            element={<Navigate to="../user" replace />}
                        />
                        <Route
                            path="users"
                            element={<Navigate to="../user" replace />}
                        />
                        <Route
                            path="hardware"
                            element={<Navigate to="../backends" replace />}
                        />
                    </Routes>
                </div>
            </main>
        </div>
    );
}
