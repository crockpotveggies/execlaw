// Settings — overlays the chat shell as a large modal with an
// internal vertical sub-sidebar listing every settings section.
//
// Routes nested under /settings/* still drive which sub-page is
// visible (deep-linking + back/forward keep working), but the
// surrounding chrome is now a modal rather than a top-level route.
// Closing the modal (× button, Escape, backdrop click, or sidebar
// nav to another route) returns the operator to /chat.

import { useCallback, useEffect, useState } from "react";
import {
    Navigate,
    NavLink,
    Route,
    Routes,
    useLocation,
    useNavigate,
} from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { setActiveThread } from "../chat/store";
import { listUiPanels, type UiPanelSummary } from "../api/endpoints";

import { GeneralPage } from "./GeneralPage";
import { PluginConfigRouter } from "./PluginConfigRouter";
import { PluginsPage } from "./PluginsPage";
import { LogsPage } from "./LogsPage";
import { EvalFlagsPage } from "./EvalFlagsPage";
import { PrincipalsPage } from "./PrincipalsPage";
import { ContactsPage } from "./ContactsPage";
import { AuditPage } from "./AuditPage";
import { BackendsPage } from "./BackendsPage";
import { UserPage } from "./UserPage";
import { ToolsPage } from "./ToolsPage";
import { McpServersPage } from "./McpServersPage";
import { PythonSandboxPage } from "./PythonSandboxPage";
import { RunnersPage } from "./RunnersPage";
import { SidecarsPage } from "./SidecarsPage";
import { ResearchPage } from "./ResearchPage";
import { SearchPage } from "./SearchPage";
import { PersonalityPage } from "./PersonalityPage";
import { AlertsPage } from "./AlertsPage";
import { TrustPolicyPage } from "./TrustPolicyPage";
import { InferencePage } from "./InferencePage";

const TABS: ReadonlyArray<{ to: string; icon: string; label: string }> = [
    { to: "/settings/general", icon: "bi-gear", label: "General" },
    { to: "/settings/user", icon: "bi-person-circle", label: "User" },
    {
        to: "/settings/personality",
        icon: "bi-chat-square-quote",
        label: "Personality",
    },
    {
        to: "/settings/research",
        icon: "bi-binoculars",
        label: "Research",
    },
    { to: "/settings/search", icon: "bi-search", label: "Search" },
    { to: "/settings/plugins", icon: "bi-plug", label: "Plugins" },
    { to: "/settings/tools", icon: "bi-wrench-adjustable", label: "Tools" },
    // Python sandbox — native feature (was a plugin until
    // 2026-05-20). Lives between Tools and MCP because operators
    // reach for it when wiring data-analysis workflows, alongside
    // the other agent-capability surfaces.
    {
        to: "/settings/python-sandbox",
        icon: "bi-filetype-py",
        label: "Python sandbox",
    },
    { to: "/settings/mcp", icon: "bi-broadcast", label: "MCP" },
    { to: "/settings/backends", icon: "bi-cpu-fill", label: "Backends" },
    {
        to: "/settings/inference",
        icon: "bi-graph-up",
        label: "Inference",
    },
    { to: "/settings/runners", icon: "bi-fire", label: "Runners" },
    { to: "/settings/sidecars", icon: "bi-boxes", label: "Sidecars" },
    { to: "/settings/contacts", icon: "bi-person-lines-fill", label: "Contacts" },
    { to: "/settings/principals", icon: "bi-people", label: "Principals" },
    { to: "/settings/trust-policy", icon: "bi-shield-lock", label: "Trust policy" },
    { to: "/settings/alerts", icon: "bi-bell", label: "Alerts" },
    { to: "/settings/logs", icon: "bi-list-columns", label: "Logs" },
    { to: "/settings/eval", icon: "bi-bar-chart", label: "Eval flags" },
    { to: "/settings/audit", icon: "bi-journal-text", label: "Audit" },
];

export function Settings() {
    const auth = useAuth();
    const navigate = useNavigate();
    const getToken = auth.getAccessToken;
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
        void auth.signOut();
    }, [auth]);

    const onNewThread = useCallback(() => {
        setActiveThread(null);
        navigate("/chat");
    }, [navigate]);

    const handleClose = useCallback(() => {
        navigate("/chat");
    }, [navigate]);

    // Escape closes the modal — matches Bootstrap modal behavior and
    // the common expectation for any full-viewport overlay.
    useEffect(() => {
        const onKeyDown = (e: KeyboardEvent) => {
            if (e.key === "Escape") handleClose();
        };
        window.addEventListener("keydown", onKeyDown);
        return () => window.removeEventListener("keydown", onKeyDown);
    }, [handleClose]);

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
        <div className="execlaw-shell">
            <Sidebar
                onNewThread={onNewThread}
                onSignOut={handleSignOut}
                uiPanels={uiPanels}
            />
            <main className="execlaw-main" aria-hidden="true">
                <header className="execlaw-main__head">
                    <h2 className="h6 mb-0">
                        <i className="bi bi-gear me-2" aria-hidden />
                        Settings
                    </h2>
                </header>
            </main>

            <div
                className="execlaw-settings-modal__backdrop"
                onClick={handleClose}
                data-testid="settings-modal-backdrop"
            />
            <div
                className="execlaw-settings-modal"
                role="dialog"
                aria-modal="true"
                aria-label="Settings"
                data-testid="settings-modal"
            >
                <nav
                    className="execlaw-settings-modal__nav"
                    aria-label="Settings sections"
                >
                    <div className="execlaw-settings-modal__nav-head">
                        <i className="bi bi-gear" aria-hidden />
                        Settings
                    </div>
                    {TABS.map((t) => (
                        <NavLink
                            key={t.to}
                            to={t.to}
                            className={({ isActive }) =>
                                "execlaw-settings-modal__nav-item" +
                                (isActive ? " is-active" : "")
                            }
                        >
                            <i className={`bi ${t.icon}`} aria-hidden />
                            {t.label}
                        </NavLink>
                    ))}
                </nav>

                <section className="execlaw-settings-modal__pane">
                    <header className="execlaw-settings-modal__head">
                        <h2 className="h6 mb-0">
                            <SettingsActiveLabel />
                        </h2>
                        <button
                            type="button"
                            className="execlaw-settings-modal__close"
                            onClick={handleClose}
                            aria-label="Close settings"
                            data-testid="settings-modal-close"
                        >
                            <i className="bi bi-x-lg" aria-hidden />
                        </button>
                    </header>
                    <div className="execlaw-settings-modal__body">
                        <Routes>
                            <Route
                                index
                                element={<Navigate to="general" replace />}
                            />
                            <Route path="general" element={<GeneralPage />} />
                            <Route path="user" element={<UserPage />} />
                            <Route
                                path="personality"
                                element={<PersonalityPage />}
                            />
                            <Route
                                path="my-identities"
                                element={<Navigate to="/settings/user" replace />}
                            />
                            <Route
                                path="routines"
                                element={<Navigate to="/routines" replace />}
                            />
                            <Route path="research" element={<ResearchPage />} />
                            <Route path="search" element={<SearchPage />} />
                            <Route path="plugins" element={<PluginsPage />} />
                            <Route
                                path="plugins/:plugin_id"
                                element={<PluginConfigRouter />}
                            />
                            <Route
                                path="google-contacts"
                                element={
                                    <Navigate
                                        to="/settings/plugins/google-contacts"
                                        replace
                                    />
                                }
                            />
                            <Route path="tools" element={<ToolsPage />} />
                            <Route
                                path="python-sandbox"
                                element={<PythonSandboxPage />}
                            />
                            <Route path="mcp" element={<McpServersPage />} />
                            <Route path="backends" element={<BackendsPage />} />
                            <Route
                                path="inference"
                                element={<InferencePage />}
                            />
                            <Route path="runners" element={<RunnersPage />} />
                            <Route path="sidecars" element={<SidecarsPage />} />
                            <Route path="contacts" element={<ContactsPage />} />
                            <Route
                                path="principals"
                                element={<PrincipalsPage />}
                            />
                            <Route
                                path="trust-policy"
                                element={<TrustPolicyPage />}
                            />
                            <Route path="alerts" element={<AlertsPage />} />
                            <Route path="logs" element={<LogsPage />} />
                            <Route path="eval" element={<EvalFlagsPage />} />
                            <Route path="audit" element={<AuditPage />} />
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
                </section>
            </div>
        </div>
    );
}

// Surfaces the active section's label inside the modal's header so
// the operator always sees what they're editing without having to
// look at the sidebar list. Falls back to "Settings" if no tab
// matches (shouldn't happen — the routes redirect to /general).
function SettingsActiveLabel() {
    const { pathname } = useLocation();
    const active = TABS.find((t) => pathname.startsWith(t.to));
    return <>{active ? active.label : "Settings"}</>;
}
