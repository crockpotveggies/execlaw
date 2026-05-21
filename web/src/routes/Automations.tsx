// /automations — top-level destination for the M4 Automations feature.
//
// Mirrors `routes/Routines.tsx`: a thin chat-shell wrapper that hosts
// the sidebar + header chrome and delegates the body to a settings-
// style page component. The route registers two paths:
//
//   * `/automations`             — landing (list + metrics + suggestions)
//   * `/automations/:id`         — detail page (definition editor + runs)

import { useCallback } from "react";
import { Navigate, useNavigate, useParams } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { setActiveThread } from "../chat/store";
import { AutomationsPage } from "../settings/AutomationsPage";
import { AutomationDetailPage } from "../settings/AutomationDetailPage";

export function Automations() {
    const auth = useAuth();
    const navigate = useNavigate();
    const { id } = useParams<{ id?: string }>();

    const onNewThread = useCallback(() => {
        setActiveThread(null);
        navigate("/chat");
    }, [navigate]);

    const onSignOut = useCallback(() => {
        void auth.signOut();
    }, [auth]);

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

    const headerTitle = id ? "Flow" : "Flows";

    return (
        <div className="execlaw-shell">
            <Sidebar onNewThread={onNewThread} onSignOut={onSignOut} />
            <main className="execlaw-main">
                <header className="execlaw-main__head">
                    <h2 className="h6 mb-0">
                        <i
                            className="bi bi-lightning-charge-fill me-2"
                            aria-hidden
                        />
                        {headerTitle}
                    </h2>
                </header>
                <div
                    className="execlaw-page execlaw-page--scroll"
                    data-testid={id ? "automation-detail-page" : "automations-page"}
                >
                    {id ? <AutomationDetailPage id={id} /> : <AutomationsPage />}
                </div>
            </main>
        </div>
    );
}
