// Single-controller login screen.
//
// One field set (username + admin password). Submits to /api/login,
// hands the resulting tokens to AuthContext.signIn, which then
// navigates to /chat once the /me probe completes.
//
// Animation: GSAP scales the card from 0.85 → 1 + fades 0 → 1 on
// mount; on successful sign-in it shrinks back to 0.85 + fades to 0
// before navigating, so the chat shell's fade-in feels like a
// continuous handoff.

import { useState, type FormEvent } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import { ApiError } from "../api/client";
import { finishWebauthnLogin, postLoginOutcome } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { useScreenTransition } from "../anim/useScreenTransition";
import { coerceRequestOptions, serializeCredential } from "../auth/webauthn";
import { useT } from "../i18n";
import { LanguageSwitcher } from "../i18n/LanguageSwitcher";

export function Login() {
    const auth = useAuth();
    const navigate = useNavigate();
    const t = useT();
    const { ref, dismiss } = useScreenTransition<HTMLDivElement>();

    const [username, setUsername] = useState("");
    const [password, setPassword] = useState("");
    const [submitError, setSubmitError] = useState<string | null>(null);
    const [submitting, setSubmitting] = useState(false);
    // Phase 7e: when the password check passes but the user has
    // webauthn registered, the server returns a challenge. We hold
    // the ceremony state here while the browser prompts for a
    // passkey. `null` = no in-flight ceremony.
    const [pendingChallenge, setPendingChallenge] = useState<
        | { ceremonyId: string; options: unknown }
        | null
    >(null);

    if (auth.status === "authenticated") {
        return <Navigate to="/chat" replace />;
    }

    const handleWebauthnAssert = async (
        ceremonyId: string,
        options: unknown,
    ) => {
        if (typeof navigator === "undefined" || !navigator.credentials) {
            setSubmitError(
                t(
                    "login.webauthnUnsupported",
                    "This browser does not support WebAuthn. Use a modern browser or remove the registered passkey.",
                ),
            );
            return;
        }
        const reqOpts = coerceRequestOptions(options);
        const cred = (await navigator.credentials.get({
            publicKey: reqOpts,
        })) as PublicKeyCredential | null;
        if (!cred) {
            setSubmitError(
                t(
                    "login.webauthnNoCredential",
                    "No credential returned from the authenticator.",
                ),
            );
            return;
        }
        const tokens = await finishWebauthnLogin(
            ceremonyId,
            serializeCredential(cred),
        );
        await auth.signIn(tokens);
        dismiss(() => navigate("/chat", { replace: true }));
    };

    const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
        e.preventDefault();
        setSubmitError(null);
        if (username.trim().length === 0 || password.length === 0) {
            setSubmitError(
                t(
                    "login.usernameAndPasswordRequired",
                    "Enter both username and password.",
                ),
            );
            return;
        }
        setSubmitting(true);
        try {
            const outcome = (await postLoginOutcome({
                username: username.trim(),
                admin_password: password,
            })) as
                | {
                      webauthn_required: false;
                      access_token: string;
                      refresh_token: string;
                  }
                | {
                      webauthn_required: true;
                      ceremony_id: string;
                      options: unknown;
                  };
            if (outcome.webauthn_required) {
                setPendingChallenge({
                    ceremonyId: outcome.ceremony_id,
                    options: outcome.options,
                });
                // Try to start the assertion ceremony immediately —
                // the user has already pressed "Sign in", they expect
                // their authenticator to prompt next.
                try {
                    await handleWebauthnAssert(
                        outcome.ceremony_id,
                        outcome.options,
                    );
                    return;
                } catch (e) {
                    // Surface a button so the operator can retry the
                    // authenticator step without re-typing the password.
                    setSubmitError(
                        e instanceof Error
                            ? t(
                                  "login.passkeyStepFailedWithReason",
                                  "Passkey step failed: {{reason}}",
                                  { reason: e.message },
                              )
                            : t(
                                  "login.passkeyStepFailed",
                                  "Passkey step failed.",
                              ),
                    );
                }
                return;
            }
            await auth.signIn({
                access_token: outcome.access_token,
                refresh_token: outcome.refresh_token,
            });
            dismiss(() => navigate("/chat", { replace: true }));
            return;
        } catch (e) {
            if (e instanceof ApiError) {
                if (e.code === "unauthorized") {
                    setSubmitError(
                        t(
                            "login.incorrectCredentials",
                            "Incorrect username or password.",
                        ),
                    );
                } else if (e.serverCode === "not_initialized") {
                    navigate("/setup", { replace: true });
                    return;
                } else {
                    setSubmitError(e.message);
                }
            } else {
                setSubmitError(
                    e instanceof Error
                        ? e.message
                        : t("login.failed", "Login failed."),
                );
            }
        } finally {
            setSubmitting(false);
        }
    };

    const onRetryWebauthn = async () => {
        if (!pendingChallenge) return;
        setSubmitError(null);
        try {
            await handleWebauthnAssert(
                pendingChallenge.ceremonyId,
                pendingChallenge.options,
            );
        } catch (e) {
            setSubmitError(
                e instanceof Error
                    ? t(
                          "login.passkeyStepFailedWithReason",
                          "Passkey step failed: {{reason}}",
                          { reason: e.message },
                      )
                    : t("login.passkeyStepFailed", "Passkey step failed."),
            );
        }
    };

    return (
        <div className="execlaw-auth-shell">
            <LanguageSwitcher />
            <div ref={ref} className="execlaw-auth-card">
                <h1 className="execlaw-brand h4 mb-1">execlaw</h1>
                <p className="execlaw-muted small mb-4">
                    {t("login.intro", "Sign in to continue.")}
                </p>

                <ErrorBanner
                    message={submitError}
                    onDismiss={() => setSubmitError(null)}
                    className="mb-3"
                    testId="login-error"
                />

                {pendingChallenge && (
                    <div className="execlaw-card mb-3" data-testid="login-webauthn-pending">
                        <div className="execlaw-card__title mb-2">
                            <i className="bi bi-key me-2" aria-hidden />
                            {t("login.passkeyRequired", "Passkey required")}
                        </div>
                        <p className="execlaw-muted small mb-3">
                            {t(
                                "login.passkeyRequiredBody",
                                "Tap your security key or use your platform authenticator to finish signing in.",
                            )}
                        </p>
                        <Button
                            type="button"
                            variant="outline-primary"
                            onClick={() => void onRetryWebauthn()}
                            data-testid="login-webauthn-retry"
                        >
                            {t("login.tryPasskeyAgain", "Try passkey again")}
                        </Button>
                    </div>
                )}

                <Form noValidate onSubmit={onSubmit}>
                    <Form.Group className="mb-3" controlId="login-username">
                        <Form.Label>{t("login.username", "Username")}</Form.Label>
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
                        <Form.Label>{t("login.password", "Password")}</Form.Label>
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
                                {t("login.signingIn", "Signing in…")}
                            </>
                        ) : (
                            t("login.signIn", "Sign in")
                        )}
                    </Button>
                </Form>
            </div>
        </div>
    );
}
