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
import { useCallback } from "react";
import { setActiveThread } from "../chat/store";
import { useNavigate } from "react-router-dom";

import { PluginsPage } from "./PluginsPage";
import { HardwarePage } from "./HardwarePage";
import { LogsPage } from "./LogsPage";
import { EvalFlagsPage } from "./EvalFlagsPage";
import { PrincipalsPage } from "./PrincipalsPage";
import { AuditPage } from "./AuditPage";

const TABS: ReadonlyArray<{ to: string; icon: string; label: string }> = [
    { to: "/settings/plugins", icon: "bi-plug", label: "Plugins" },
    { to: "/settings/principals", icon: "bi-people", label: "Principals" },
    { to: "/settings/hardware", icon: "bi-cpu", label: "Hardware" },
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
            <Sidebar onNewThread={onNewThread} onSignOut={handleSignOut} />
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
                            element={<Navigate to="plugins" replace />}
                        />
                        <Route path="plugins" element={<PluginsPage />} />
                        <Route path="principals" element={<PrincipalsPage />} />
                        <Route path="hardware" element={<HardwarePage />} />
                        <Route path="logs" element={<LogsPage />} />
                        <Route path="eval" element={<EvalFlagsPage />} />
                        <Route path="audit" element={<AuditPage />} />
                    </Routes>
                </div>
            </main>
        </div>
    );
}
