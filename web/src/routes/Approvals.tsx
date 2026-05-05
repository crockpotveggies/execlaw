// /approvals — destination for cold-contact decisions.
//
// First-class entry alongside /chat, /routines, /research. The
// sidebar already surfaces a count badge when there are pending
// approvals; this page is what that badge links to. Each pending
// approval renders as a card with the original message preview +
// the same five action buttons the inline ApprovalCard uses
// (trust, trust_limited, claim_as_me, ignore_once, block).
//
// Approving here also re-dispatches the queued message through
// `dispatch_external_turn` server-side so the agent answers what
// was actually asked — no need to go ask the contact to re-send.

import { useCallback } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { Sidebar } from "../chat/Sidebar";
import { setActiveThread } from "../chat/store";
import { ApprovalsPage } from "../approvals/ApprovalsPage";

export function Approvals() {
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
                        <i className="bi bi-shield-exclamation me-2" aria-hidden />
                        Approvals
                    </h2>
                </header>
                <div
                    className="execlaw-page execlaw-page--scroll"
                    data-testid="approvals-page"
                >
                    <ApprovalsPage />
                </div>
            </main>
        </div>
    );
}

