// /skills — top-level destination, sibling of Chat / Routines /
// Research / Settings.
//
// Skills are procedural-knowledge documents the agent loads on demand
// to shape its behavior. Promoted to a first-class page (rather than
// a Settings tab) because they're an operator-facing artifact like
// routines and research jobs, not a low-traffic config knob.

import { useCallback } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { setActiveThread } from "../chat/store";
import { SkillsPage } from "../skills/SkillsPage";

export function Skills() {
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
                        <i className="bi bi-stars me-2" aria-hidden />
                        Skills
                    </h2>
                </header>
                <SkillsPage />
            </main>
        </div>
    );
}
