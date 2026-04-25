// Chat placeholder.
//
// The full chat shell (sidebar with thread list + Control thread merge,
// message stream, inline approval card) lands in Phase-6a's next pass
// once the scaffold + auth flow are validated end-to-end.
//
// For now this screen renders a minimal "you're signed in" view that
// echoes the /api/admin/me payload, plus a sign-out button — enough
// to confirm the entire setup → JWT → /me round-trip works.

import Button from "react-bootstrap/Button";
import { Navigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";

export function Chat() {
    const auth = useAuth();

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

    const user = auth.user!;

    return (
        <div className="execlaw-auth-shell">
            <div className="execlaw-auth-card">
                <h1 className="execlaw-brand h4 mb-3">execlaw</h1>
                <p className="mb-1">
                    Signed in as <strong>{user.display_name}</strong>
                </p>
                {user.email && (
                    <p className="execlaw-muted small mb-1">{user.email}</p>
                )}
                <p className="execlaw-muted small mb-4">
                    Role: {user.role} · Principal id:{" "}
                    <code>{user.user_id}</code>
                </p>

                <div className="execlaw-muted small mb-4">
                    <i className="bi bi-chat-square-text me-2" aria-hidden />
                    Chat shell coming next — thread list, streaming
                    tokens, inline approvals.
                </div>

                <Button variant="outline-secondary" onClick={auth.signOut}>
                    <i className="bi bi-box-arrow-right me-2" aria-hidden />
                    Sign out
                </Button>
            </div>
        </div>
    );
}
