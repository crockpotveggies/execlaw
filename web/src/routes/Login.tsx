// Single-controller login screen.
//
// One field (admin password). Submits to /api/login, hands the
// resulting tokens to AuthContext.signIn, which then navigates to
// /chat once the /me probe completes.

import { useState, type FormEvent } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import { ApiError } from "../api/client";
import { postLogin } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

export function Login() {
    const auth = useAuth();
    const navigate = useNavigate();

    const [username, setUsername] = useState("");
    const [password, setPassword] = useState("");
    const [submitError, setSubmitError] = useState<string | null>(null);
    const [submitting, setSubmitting] = useState(false);

    if (auth.status === "authenticated") {
        return <Navigate to="/chat" replace />;
    }

    const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
        e.preventDefault();
        setSubmitError(null);
        if (username.trim().length === 0 || password.length === 0) {
            setSubmitError("Enter both username and password.");
            return;
        }
        setSubmitting(true);
        try {
            const tokens = await postLogin({
                username: username.trim(),
                admin_password: password,
            });
            await auth.signIn(tokens);
            navigate("/chat", { replace: true });
        } catch (e) {
            if (e instanceof ApiError) {
                // Server returns the same `bad_credentials` for both
                // wrong-username and wrong-password — surface a single
                // generic message so the SPA doesn't re-leak it.
                if (e.code === "unauthorized") {
                    setSubmitError("Incorrect username or password.");
                } else if (e.serverCode === "not_initialized") {
                    navigate("/setup", { replace: true });
                    return;
                } else {
                    setSubmitError(e.message);
                }
            } else {
                setSubmitError(
                    e instanceof Error ? e.message : "Login failed.",
                );
            }
        } finally {
            setSubmitting(false);
        }
    };

    return (
        <div className="execlaw-auth-shell">
            <div className="execlaw-auth-card">
                <h1 className="execlaw-brand h4 mb-1">execlaw</h1>
                <p className="execlaw-muted small mb-4">Sign in to continue.</p>

                {submitError && (
                    <div
                        className="execlaw-error-banner mb-3"
                        role="alert"
                        data-testid="login-error"
                    >
                        {submitError}
                    </div>
                )}

                <Form noValidate onSubmit={onSubmit}>
                    <Form.Group className="mb-3" controlId="login-username">
                        <Form.Label>Username</Form.Label>
                        <Form.Control
                            type="text"
                            autoComplete="username"
                            value={username}
                            onChange={(e) => setUsername(e.target.value)}
                            disabled={submitting}
                            autoFocus
                            spellCheck={false}
                            autoCapitalize="none"
                        />
                    </Form.Group>

                    <Form.Group className="mb-4" controlId="login-password">
                        <Form.Label>Password</Form.Label>
                        <Form.Control
                            type="password"
                            autoComplete="current-password"
                            value={password}
                            onChange={(e) => setPassword(e.target.value)}
                            disabled={submitting}
                        />
                    </Form.Group>

                    <Button
                        type="submit"
                        variant="primary"
                        className="w-100"
                        disabled={submitting}
                    >
                        {submitting ? (
                            <>
                                <Spinner size="sm" animation="border" className="me-2" />
                                Signing in…
                            </>
                        ) : (
                            "Sign in"
                        )}
                    </Button>
                </Form>
            </div>
        </div>
    );
}
