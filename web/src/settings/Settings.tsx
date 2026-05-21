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
    // General first (Phase 14 bare-metal pivot) — host-service knobs
    // like start-on-boot + bind address. These are operator settings
    // about the install itself, before any per-feature configuration.
    { to: "/settings/general", icon: "bi-gear", label: "General" },
    // User — operator accounts, password, passkeys, sessions.
    // Singular "User" because it's the page about *your* account
    // (other operators are listed inside it for Controllers). Was
    // briefly called "Login" but that read as the sign-in screen.
    { to: "/settings/user", icon: "bi-person-circle", label: "User" },
    // Personality — operator-editable half of the system prompt
    // (Phase 9, MIGRATION_PLAN §5.5). Distinct from User because it's
    // about the AGENT's voice, not the operator's account.
    {
        to: "/settings/personality",
        icon: "bi-chat-square-quote",
        label: "Personality",
    },
    // My identities used to be its own tab; merged into the User
    // page (Phase 14, 2026-05-02). A redirect below keeps old
    // bookmarks pointed at `/settings/my-identities` working.
    // Routines used to live here too (Phase 10) but it's a feature,
    // not a setting — it now sits at the top-level `/routines` route
    // with its own sidebar entry. A back-compat redirect below keeps
    // old `/settings/routines` bookmarks working.
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
    // Hardware now lives at the bottom of the Backends page.
    { to: "/settings/backends", icon: "bi-cpu-fill", label: "Backends" },
    // Inference — M5 per-consumer observability. Lives alongside
    // Backends because operators reach for both when diagnosing
    // "is the model the bottleneck" questions.
    {
        to: "/settings/inference",
        icon: "bi-graph-up",
        label: "Inference",
    },
    { to: "/settings/runners", icon: "bi-fire", label: "Runners" },
    // Sidecars — companion containers the sidecar supervisor manages
    // (signal-cli, WhatsApp bridges, future OCR / ffmpeg helpers).
    // Sits next to Runners because both are container-supervisor surfaces;
    // operators tend to reach for them in the same diagnosis sessions.
    { to: "/settings/sidecars", icon: "bi-boxes", label: "Sidecars" },
    // Contacts is the curated address-book view, Principals is the
    // "everything else" view (controllers, delegated bots, blocked).
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
    const { ref: shellRef, dismiss } = useScreenTransition<HTMLDivElement>({
        initialScale: 1,
        exitScale: 1,
        durationMs: 220,
    });
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
                            element={<Navigate to="general" replace />}
                        />
                        <Route path="general" element={<GeneralPage />} />
                        <Route path="user" element={<UserPage />} />
                        <Route path="personality" element={<PersonalityPage />} />
                        {/* Legacy: my-identities is now a panel
                            inside the User page. */}
                        <Route
                            path="my-identities"
                            element={<Navigate to="/settings/user" replace />}
                        />
                        {/* Legacy: routines is now a top-level page. */}
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
                        {/* Legacy direct path → redirect via the
                            plugin-config router so old bookmarks
                            still land in the right place. */}
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
                        <Route path="inference" element={<InferencePage />} />
                        <Route path="runners" element={<RunnersPage />} />
                        <Route path="sidecars" element={<SidecarsPage />} />
                        <Route path="contacts" element={<ContactsPage />} />
                        <Route path="principals" element={<PrincipalsPage />} />
                        <Route path="trust-policy" element={<TrustPolicyPage />} />
                        <Route path="alerts" element={<AlertsPage />} />
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
