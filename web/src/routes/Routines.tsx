// /routines — top-level destination, not a buried setting.
//
// Routines are agent automations the operator interacts with as a
// first-class feature, so they get their own URL + sidebar entry
// instead of living inside /settings/*. The actual editor body
// stays in `settings/RoutinesPage.tsx` (it pre-dates this move and
// still has its own tests there); this file just supplies the
// chat-shell chrome (sidebar + page header) the same way
// `routes/Research.tsx` does for /research.

import { useCallback } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { setActiveThread } from "../chat/store";
import { RoutinesPage } from "../settings/RoutinesPage";

export function Routines() {
    const auth = useAuth();
    const navigate = useNavigate();

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

    return (
        <div className="execlaw-shell">
            <Sidebar onNewThread={onNewThread} onSignOut={onSignOut} />
            <main className="execlaw-main">
                <header className="execlaw-main__head">
                    <h2 className="h6 mb-0">
                        <i className="bi bi-clock-history me-2" aria-hidden />
                        Routines
                    </h2>
                </header>
                <div
                    className="execlaw-page execlaw-page--scroll"
                    data-testid="routines-page"
                >
                    <RoutinesPage />
                </div>
            </main>
        </div>
    );
}
