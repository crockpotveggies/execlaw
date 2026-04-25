// First-run setup wizard.
//
// One screen, three fields:
//   - display name (required)
//   - admin password (required, ≥ 8 chars)
//   - email (optional)
//
// Posts to `/api/setup`, then signs the new tokens into the auth
// context which navigates to /chat. If the server reports
// `already_initialized` (409), we send the user to /login instead —
// covers the edge case of two browser tabs racing the wizard.

import { useState, type FormEvent } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import { ApiError } from "../api/client";
import { postSetup } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

interface FieldErrors {
    username?: string;
    display_name?: string;
    admin_password?: string;
    email?: string;
}

const PASSWORD_MIN_LEN = 8;
const USERNAME_MIN_LEN = 3;
const USERNAME_MAX_LEN = 32;
const USERNAME_PATTERN = /^[a-zA-Z0-9_-]+$/;

export function validateSetupForm(input: {
    username: string;
    display_name: string;
    admin_password: string;
    email: string;
}): FieldErrors {
    const errors: FieldErrors = {};

    const trimmedUsername = input.username.trim();
    if (trimmedUsername.length === 0) {
        errors.username = "Required.";
    } else if (trimmedUsername.length < USERNAME_MIN_LEN) {
        errors.username = `Must be at least ${USERNAME_MIN_LEN} characters.`;
    } else if (trimmedUsername.length > USERNAME_MAX_LEN) {
        errors.username = `Must be at most ${USERNAME_MAX_LEN} characters.`;
    } else if (!USERNAME_PATTERN.test(trimmedUsername)) {
        errors.username = "Letters, digits, underscore, hyphen only.";
    }

    if (input.display_name.trim().length === 0) {
        errors.display_name = "Required.";
    }
    if (input.admin_password.length < PASSWORD_MIN_LEN) {
        errors.admin_password = `Must be at least ${PASSWORD_MIN_LEN} characters.`;
    }
    if (input.email.trim().length > 0) {
        // Loose RFC-ish check — the backend doesn't validate either,
        // so this just stops obvious typos rather than enforcing a
        // canonical form.
        if (!/^\S+@\S+\.\S+$/.test(input.email.trim())) {
            errors.email = "Doesn't look like an email address.";
        }
    }
    return errors;
}

export function SetupWizard() {
    const auth = useAuth();
    const navigate = useNavigate();

    const [username, setUsername] = useState("");
    const [displayName, setDisplayName] = useState("");
    const [password, setPassword] = useState("");
    const [email, setEmail] = useState("");
    const [errors, setErrors] = useState<FieldErrors>({});
    const [submitError, setSubmitError] = useState<string | null>(null);
    const [submitting, setSubmitting] = useState(false);

    if (auth.status === "authenticated") {
        // A logged-in operator landed here by URL; just bounce them.
        return <Navigate to="/chat" replace />;
    }

    const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
        e.preventDefault();
        setSubmitError(null);
        const fieldErrs = validateSetupForm({
            username,
            display_name: displayName,
            admin_password: password,
            email,
        });
        setErrors(fieldErrs);
        if (Object.keys(fieldErrs).length > 0) return;

        setSubmitting(true);
        try {
            const trimmedEmail = email.trim();
            const resp = await postSetup({
                username: username.trim(),
                admin_password: password,
                display_name: displayName.trim(),
                ...(trimmedEmail.length > 0 ? { email: trimmedEmail } : {}),
            });
            await auth.signIn({
                access_token: resp.access_token,
                refresh_token: resp.refresh_token,
            });
            navigate("/chat", { replace: true });
        } catch (e) {
            if (e instanceof ApiError && e.serverCode === "already_initialized") {
                navigate("/login", { replace: true });
                return;
            }
            setSubmitError(
                e instanceof Error ? e.message : "Setup failed; try again.",
            );
        } finally {
            setSubmitting(false);
        }
    };

    return (
        <div className="execlaw-auth-shell">
            <div className="execlaw-auth-card">
                <h1 className="execlaw-brand h4 mb-1">execlaw</h1>
                <p className="execlaw-muted small mb-4">
                    Welcome — let&rsquo;s create your controller account.
                </p>

                {submitError && (
                    <div
                        className="execlaw-error-banner mb-3"
                        role="alert"
                        data-testid="setup-submit-error"
                    >
                        {submitError}
                    </div>
                )}

                <Form noValidate onSubmit={onSubmit}>
                    <Form.Group className="mb-3" controlId="setup-username">
                        <Form.Label>Username</Form.Label>
                        <Form.Control
                            type="text"
                            autoComplete="username"
                            value={username}
                            onChange={(e) => setUsername(e.target.value)}
                            isInvalid={!!errors.username}
                            disabled={submitting}
                            autoFocus
                            spellCheck={false}
                            autoCapitalize="none"
                        />
                        <Form.Control.Feedback type="invalid">
                            {errors.username}
                        </Form.Control.Feedback>
                        <Form.Text className="execlaw-muted">
                            Used to sign in. Letters, digits, underscore, hyphen.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-3" controlId="setup-display-name">
                        <Form.Label>Display name</Form.Label>
                        <Form.Control
                            type="text"
                            autoComplete="name"
                            value={displayName}
                            onChange={(e) => setDisplayName(e.target.value)}
                            isInvalid={!!errors.display_name}
                            disabled={submitting}
                        />
                        <Form.Control.Feedback type="invalid">
                            {errors.display_name}
                        </Form.Control.Feedback>
                    </Form.Group>

                    <Form.Group className="mb-3" controlId="setup-password">
                        <Form.Label>Admin password</Form.Label>
                        <Form.Control
                            type="password"
                            autoComplete="new-password"
                            value={password}
                            onChange={(e) => setPassword(e.target.value)}
                            isInvalid={!!errors.admin_password}
                            disabled={submitting}
                        />
                        <Form.Control.Feedback type="invalid">
                            {errors.admin_password}
                        </Form.Control.Feedback>
                        <Form.Text className="execlaw-muted">
                            At least {PASSWORD_MIN_LEN} characters. You can change it later.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-4" controlId="setup-email">
                        <Form.Label>
                            Email <span className="execlaw-muted">(optional)</span>
                        </Form.Label>
                        <Form.Control
                            type="email"
                            autoComplete="email"
                            value={email}
                            onChange={(e) => setEmail(e.target.value)}
                            isInvalid={!!errors.email}
                            disabled={submitting}
                        />
                        <Form.Control.Feedback type="invalid">
                            {errors.email}
                        </Form.Control.Feedback>
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
                                Creating…
                            </>
                        ) : (
                            "Create account"
                        )}
                    </Button>
                </Form>
            </div>
        </div>
    );
}
