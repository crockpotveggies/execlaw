// Settings → User (Phase 8.7 rename + section reorder).
//
// Consolidated security / auth / account page. Sections,
// top-to-bottom:
//
//   1. **Operator accounts** — Controllers see the multi-user list
//      with invite / delete + per-row password reset. Non-
//      Controllers see a read-only view.
//   2. **Your password** — change-password form. Requires the
//      current password as proof of identity; rejected by the
//      server if the new one is < 8 chars.
//   3. **Passkeys** — list / add / remove WebAuthn credentials.
//      Stub-mode notice when the server build doesn't include the
//      `webauthn` feature (Windows-host dev binary, etc.).
//   4. **Sessions** — Sign out everywhere. Revokes every refresh
//      token bound to the caller's user_id.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import { ApiError } from "../api/client";
import {
    beginWebauthnRegistration,
    changeMyPassword,
    deleteUser,
    deleteWebauthnCredential,
    finishWebauthnRegistration,
    inviteUser,
    listUsers,
    listWebauthnCredentials,
    resetUserPassword,
    type UserRole,
    type UserView,
    type WebauthnCredentialView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import {
    coerceCreationOptions,
    serializeCredential,
} from "../auth/webauthn";

const ROLES: ReadonlyArray<UserRole> = ["operator", "viewer", "controller"];

interface InviteFormState {
    username: string;
    display_name: string;
    initial_password: string;
    role: UserRole;
    email: string;
}
const EMPTY_INVITE: InviteFormState = {
    username: "",
    display_name: "",
    initial_password: "",
    role: "operator",
    email: "",
};

export function UserPage() {
    const auth = useAuth();
    const navigate = useNavigate();
    const getToken = auth.getAccessToken;

    const meRole = auth.user?.role ?? "viewer";
    const meId = auth.user?.user_id ?? null;
    const canMutate = meRole === "controller";

    return (
        <div data-testid="settings-user">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">User</h3>
            </div>
            <p className="execlaw-muted small mb-3">
                Operator accounts, your password, your passkeys, and
                active sessions — everything that controls how you (and
                other operators) sign in.
            </p>

            <OperatorsCard
                getToken={getToken}
                canMutate={canMutate}
                meId={meId}
            />
            <ChangePasswordCard getToken={getToken} />
            <PasskeysCard getToken={getToken} />
            <SessionsCard
                signOutEverywhere={auth.signOutEverywhere}
                navigate={navigate}
            />
        </div>
    );
}

// ---------------------------------------------------------------------------
// 1. Change password
// ---------------------------------------------------------------------------

function ChangePasswordCard({ getToken }: { getToken: () => string | null }) {
    const [current, setCurrent] = useState("");
    const [next, setNext] = useState("");
    const [confirm, setConfirm] = useState("");
    const [error, setError] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);

    const onSubmit = useCallback(
        async (e: React.FormEvent) => {
            e.preventDefault();
            setError(null);
            setNotice(null);
            if (next.length < 8) {
                setError("New password must be at least 8 characters.");
                return;
            }
            if (next !== confirm) {
                setError("New password and confirmation don't match.");
                return;
            }
            setBusy(true);
            try {
                await changeMyPassword(
                    { current_password: current, new_password: next },
                    getToken,
                );
                setCurrent("");
                setNext("");
                setConfirm("");
                setNotice("Password updated.");
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [current, next, confirm, getToken],
    );

    return (
        <div className="execlaw-card mb-3" data-testid="user-password-card">
            <div className="execlaw-card__title mb-2">
                <i className="bi bi-shield-lock me-2" aria-hidden />
                Your password
            </div>
            <p className="execlaw-muted small mb-3">
                Rotate your password. Existing sessions on other devices
                are NOT signed out automatically — use the Sessions
                section below to sign out everywhere.
            </p>
            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-2" />
            {notice && (
                <div className="execlaw-muted small mb-2" role="status">
                    {notice}
                </div>
            )}
            <Form onSubmit={onSubmit}>
                <div className="row g-2">
                    <Form.Group className="col-sm-6">
                        <Form.Label className="execlaw-muted small mb-1">
                            Current password
                        </Form.Label>
                        <Form.Control
                            type="password"
                            value={current}
                            autoComplete="current-password"
                            onChange={(e) => setCurrent(e.target.value)}
                            disabled={busy}
                            data-testid="user-password-current"
                        />
                    </Form.Group>
                </div>
                <div className="row g-2 mt-1">
                    <Form.Group className="col-sm-6">
                        <Form.Label className="execlaw-muted small mb-1">
                            New password
                        </Form.Label>
                        <Form.Control
                            type="password"
                            value={next}
                            autoComplete="new-password"
                            onChange={(e) => setNext(e.target.value)}
                            disabled={busy}
                            data-testid="user-password-new"
                        />
                    </Form.Group>
                    <Form.Group className="col-sm-6">
                        <Form.Label className="execlaw-muted small mb-1">
                            Confirm new password
                        </Form.Label>
                        <Form.Control
                            type="password"
                            value={confirm}
                            autoComplete="new-password"
                            onChange={(e) => setConfirm(e.target.value)}
                            disabled={busy}
                            data-testid="user-password-confirm"
                        />
                    </Form.Group>
                </div>
                <div className="mt-3">
                    <Button
                        type="submit"
                        variant="primary"
                        disabled={
                            busy ||
                            current.length === 0 ||
                            next.length === 0 ||
                            confirm.length === 0
                        }
                        data-testid="user-password-submit"
                    >
                        Update password
                    </Button>
                </div>
            </Form>
        </div>
    );
}

// ---------------------------------------------------------------------------
// 2. Passkeys
// ---------------------------------------------------------------------------

// Detect "the server build doesn't include webauthn" via the
// structured server code that the stub-mode router emits. Looking at
// the message text was fragile — the message is "the server was built
// without webauthn support," which doesn't contain the literal token
// "webauthn_unconfigured". Use ApiError.serverCode instead.
function isWebauthnUnconfigured(e: unknown): boolean {
    return (
        e instanceof ApiError &&
        (e.serverCode === "webauthn_unconfigured" || e.status === 503)
    );
}

function PasskeysCard({ getToken }: { getToken: () => string | null }) {
    const [creds, setCreds] = useState<WebauthnCredentialView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [unsupported, setUnsupported] = useState(false);
    const [busy, setBusy] = useState(false);
    const [label, setLabel] = useState("");

    const refresh = useCallback(async () => {
        try {
            const r = await listWebauthnCredentials(getToken);
            setCreds(r.credentials);
            setUnsupported(false);
            setError(null);
        } catch (e) {
            if (isWebauthnUnconfigured(e)) {
                setCreds([]);
                setUnsupported(true);
                setError(null);
                return;
            }
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onAdd = useCallback(async () => {
        if (label.trim().length === 0) {
            setError("Label is required so you can identify the device.");
            return;
        }
        if (typeof navigator === "undefined" || !navigator.credentials) {
            setError(
                "This browser does not support WebAuthn. Use a modern browser to register a passkey.",
            );
            return;
        }
        setBusy(true);
        setError(null);
        try {
            const begin = await beginWebauthnRegistration(label.trim(), getToken);
            const opts = coerceCreationOptions(begin.options);
            const cred = (await navigator.credentials.create({
                publicKey: opts,
            })) as PublicKeyCredential | null;
            if (!cred) throw new Error("No credential returned from authenticator.");
            await finishWebauthnRegistration(
                begin.ceremony_id,
                serializeCredential(cred),
                getToken,
            );
            setLabel("");
            await refresh();
        } catch (e) {
            if (isWebauthnUnconfigured(e)) {
                setUnsupported(true);
                setError(null);
            } else {
                setError(e instanceof Error ? e.message : String(e));
            }
        } finally {
            setBusy(false);
        }
    }, [getToken, label, refresh]);

    const onDelete = useCallback(
        async (cred: WebauthnCredentialView) => {
            if (
                !confirm(
                    `Remove passkey "${cred.label}"? You will need at least one factor to sign in.`,
                )
            )
                return;
            try {
                await deleteWebauthnCredential(cred.credential_id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken, refresh],
    );

    return (
        <div className="execlaw-card mb-3" data-testid="user-passkeys-card">
            <div className="execlaw-card__title mb-2">
                <i className="bi bi-key me-2" aria-hidden />
                Passkeys (WebAuthn)
            </div>
            <p className="execlaw-muted small mb-3">
                Register a security key or platform authenticator as a
                second factor. Sign-in still requires your password —
                the passkey is checked after.
            </p>
            {unsupported && (
                <div
                    className="execlaw-card mb-3"
                    role="status"
                    data-testid="user-passkeys-unsupported"
                    style={{ background: "rgba(255,193,7,0.07)" }}
                >
                    <strong>Passkeys aren't enabled in this build.</strong>
                    <div className="execlaw-muted small mt-1">
                        The server you're connected to was built without the{" "}
                        <code>webauthn</code> Cargo feature — usually because
                        this is a Windows-host dev binary where{" "}
                        <code>openssl-sys</code> can't compile out of the
                        box. The production Docker image (Linux) builds with
                        the feature on. Test the passkey flow there, or
                        rebuild the dev binary with{" "}
                        <code>--features webauthn</code> on a host with
                        OpenSSL headers available.
                    </div>
                </div>
            )}
            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />
            <div className="d-flex gap-2 mb-3">
                <Form.Control
                    value={label}
                    onChange={(e) => setLabel(e.target.value)}
                    placeholder="Label this passkey (e.g. YubiKey 5C)"
                    disabled={busy || unsupported}
                    data-testid="user-passkey-label"
                />
                <Button
                    variant="primary"
                    disabled={busy || unsupported || label.trim().length === 0}
                    onClick={() => void onAdd()}
                    data-testid="user-passkey-add"
                >
                    Add passkey
                </Button>
            </div>
            {creds === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : creds.length === 0 ? (
                <div className="execlaw-muted small">
                    No passkeys registered. Sign-in will require only your
                    password.
                </div>
            ) : (
                <ul className="list-unstyled m-0">
                    {creds.map((c) => (
                        <li
                            key={c.credential_id}
                            className="d-flex align-items-center py-2 border-top"
                            data-testid="user-passkey-row"
                            data-credential-id={c.credential_id}
                        >
                            <div className="flex-grow-1">
                                <div>{c.label}</div>
                                <div className="execlaw-muted small">
                                    Added {new Date(c.created_at * 1000).toLocaleDateString()}
                                    {c.last_used_at && (
                                        <>
                                            {" · last used "}
                                            {new Date(
                                                c.last_used_at * 1000,
                                            ).toLocaleDateString()}
                                        </>
                                    )}
                                </div>
                            </div>
                            <Button
                                size="sm"
                                variant="outline-danger"
                                onClick={() => void onDelete(c)}
                                data-testid="user-passkey-remove"
                            >
                                Remove
                            </Button>
                        </li>
                    ))}
                </ul>
            )}
        </div>
    );
}

// ---------------------------------------------------------------------------
// 3. Sessions
// ---------------------------------------------------------------------------

function SessionsCard({
    signOutEverywhere,
    navigate,
}: {
    signOutEverywhere: () => Promise<{ revokedCount: number }>;
    navigate: (path: string, opts?: { replace?: boolean }) => void;
}) {
    const [busy, setBusy] = useState(false);
    const [notice, setNotice] = useState<string | null>(null);

    const onSignOutAll = useCallback(async () => {
        if (
            !confirm(
                "Sign out of every other browser and device? You'll have to log back in everywhere else.",
            )
        )
            return;
        setBusy(true);
        try {
            const r = await signOutEverywhere();
            setNotice(
                `Signed out of ${r.revokedCount} session${r.revokedCount === 1 ? "" : "s"}.`,
            );
            navigate("/login", { replace: true });
        } finally {
            setBusy(false);
        }
    }, [signOutEverywhere, navigate]);

    return (
        <div className="execlaw-card mb-3" data-testid="user-sessions-card">
            <div className="execlaw-card__title mb-2">
                <i className="bi bi-box-arrow-right me-2" aria-hidden />
                Sessions
            </div>
            <p className="execlaw-muted small mb-3">
                Sign yourself out of every other browser and device.
                Useful if a device is lost or you suspect a token leak.
            </p>
            {notice && (
                <div className="execlaw-muted small mb-2">{notice}</div>
            )}
            <Button
                variant="outline-danger"
                disabled={busy}
                onClick={() => void onSignOutAll()}
                data-testid="user-sign-out-everywhere"
            >
                Sign out everywhere
            </Button>
        </div>
    );
}

// ---------------------------------------------------------------------------
// 4. Operator accounts (the old Users page)
// ---------------------------------------------------------------------------

function OperatorsCard({
    getToken,
    canMutate,
    meId,
}: {
    getToken: () => string | null;
    canMutate: boolean;
    meId: string | null;
}) {
    const [users, setUsers] = useState<UserView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    const [inviting, setInviting] = useState(false);
    const [inviteForm, setInviteForm] = useState<InviteFormState>(EMPTY_INVITE);
    const [inviteError, setInviteError] = useState<string | null>(null);
    const [resetTarget, setResetTarget] = useState<string | null>(null);
    const [resetPassword, setResetPassword] = useState("");
    const [resetError, setResetError] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listUsers(getToken);
            setUsers(r.users);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onInvite = useCallback(
        async (e: React.FormEvent) => {
            e.preventDefault();
            setInviteError(null);
            try {
                await inviteUser(
                    {
                        username: inviteForm.username.trim(),
                        display_name: inviteForm.display_name.trim(),
                        initial_password: inviteForm.initial_password,
                        role: inviteForm.role,
                        ...(inviteForm.email.trim().length > 0
                            ? { email: inviteForm.email.trim() }
                            : {}),
                    },
                    getToken,
                );
                setInviteForm(EMPTY_INVITE);
                setInviting(false);
                await refresh();
            } catch (err) {
                setInviteError(err instanceof Error ? err.message : String(err));
            }
        },
        [getToken, inviteForm, refresh],
    );

    const onDelete = useCallback(
        async (u: UserView) => {
            if (
                !confirm(
                    `Remove ${u.display_name} (@${u.username})? They will no longer be able to sign in.`,
                )
            )
                return;
            setBusyId(u.user_id);
            try {
                await deleteUser(u.user_id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    const onSubmitReset = useCallback(
        async (e: React.FormEvent) => {
            e.preventDefault();
            if (!resetTarget) return;
            setResetError(null);
            if (resetPassword.length < 8) {
                setResetError("New password must be at least 8 characters.");
                return;
            }
            try {
                await resetUserPassword(
                    resetTarget,
                    { new_password: resetPassword },
                    getToken,
                );
                setResetTarget(null);
                setResetPassword("");
            } catch (e) {
                setResetError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken, resetTarget, resetPassword],
    );

    const roles = useMemo(() => ROLES, []);

    return (
        <div className="execlaw-card mb-3" data-testid="user-operators-card">
            <div className="d-flex align-items-center mb-2">
                <div className="execlaw-card__title flex-grow-1">
                    <i className="bi bi-person-gear me-2" aria-hidden />
                    Operator accounts
                </div>
                {canMutate && !inviting && (
                    <Button
                        size="sm"
                        variant="primary"
                        onClick={() => {
                            setInviting(true);
                            setInviteError(null);
                            setInviteForm(EMPTY_INVITE);
                        }}
                        data-testid="user-invite"
                    >
                        <i className="bi bi-person-plus me-2" aria-hidden />
                        Invite user
                    </Button>
                )}
            </div>

            {!canMutate && (
                <div className="execlaw-muted small mb-2">
                    Read-only view. Only Controllers can invite, remove, or
                    reset other operators' passwords.
                </div>
            )}
            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-2" />

            {inviting && (
                <Form
                    className="mb-3"
                    onSubmit={onInvite}
                    data-testid="user-invite-form"
                >
                    <ErrorBanner message={inviteError} onDismiss={() => setInviteError(null)} className="mb-2" />
                    <div className="row g-2">
                        <Form.Group className="col-sm-4">
                            <Form.Label className="execlaw-muted small mb-1">
                                Username
                            </Form.Label>
                            <Form.Control
                                value={inviteForm.username}
                                onChange={(e) =>
                                    setInviteForm({
                                        ...inviteForm,
                                        username: e.target.value,
                                    })
                                }
                                spellCheck={false}
                                autoCapitalize="none"
                                data-testid="user-invite-username"
                            />
                        </Form.Group>
                        <Form.Group className="col-sm-4">
                            <Form.Label className="execlaw-muted small mb-1">
                                Display name
                            </Form.Label>
                            <Form.Control
                                value={inviteForm.display_name}
                                onChange={(e) =>
                                    setInviteForm({
                                        ...inviteForm,
                                        display_name: e.target.value,
                                    })
                                }
                            />
                        </Form.Group>
                        <Form.Group className="col-sm-4">
                            <Form.Label className="execlaw-muted small mb-1">
                                Role
                            </Form.Label>
                            <Form.Select
                                value={inviteForm.role}
                                onChange={(e) =>
                                    setInviteForm({
                                        ...inviteForm,
                                        role: e.target.value as UserRole,
                                    })
                                }
                                data-testid="user-invite-role"
                            >
                                {roles.map((r) => (
                                    <option key={r} value={r}>
                                        {r}
                                    </option>
                                ))}
                            </Form.Select>
                        </Form.Group>
                        <Form.Group className="col-sm-6">
                            <Form.Label className="execlaw-muted small mb-1">
                                Initial password
                            </Form.Label>
                            <Form.Control
                                type="password"
                                value={inviteForm.initial_password}
                                onChange={(e) =>
                                    setInviteForm({
                                        ...inviteForm,
                                        initial_password: e.target.value,
                                    })
                                }
                                data-testid="user-invite-password"
                            />
                            <Form.Text className="execlaw-muted">
                                At least 8 characters. They can rotate it from
                                their own Login page.
                            </Form.Text>
                        </Form.Group>
                        <Form.Group className="col-sm-6">
                            <Form.Label className="execlaw-muted small mb-1">
                                Email (optional)
                            </Form.Label>
                            <Form.Control
                                type="email"
                                value={inviteForm.email}
                                onChange={(e) =>
                                    setInviteForm({
                                        ...inviteForm,
                                        email: e.target.value,
                                    })
                                }
                            />
                        </Form.Group>
                    </div>
                    <div className="d-flex gap-2 mt-2">
                        <Button
                            type="submit"
                            variant="primary"
                            data-testid="user-invite-submit"
                        >
                            Invite
                        </Button>
                        <Button
                            variant="outline-secondary"
                            onClick={() => setInviting(false)}
                        >
                            Cancel
                        </Button>
                    </div>
                </Form>
            )}

            {users === null ? (
                <div className="execlaw-muted small">Loading users…</div>
            ) : (
                users.map((u) => {
                    const isMe = meId === u.user_id;
                    return (
                        <div
                            key={u.user_id}
                            className="d-flex align-items-center py-2 border-top"
                            data-testid="user-user-row"
                            data-user-id={u.user_id}
                        >
                            <div className="flex-grow-1">
                                <div>
                                    {u.display_name}
                                    <span className="execlaw-muted ms-2">
                                        @{u.username}
                                    </span>
                                    {isMe && (
                                        <span className="execlaw-trust-badge ms-2 is-controller">
                                            you
                                        </span>
                                    )}
                                    <span
                                        className={`execlaw-trust-badge ms-2 ${roleBadgeClass(u.role)}`}
                                    >
                                        {u.role}
                                    </span>
                                </div>
                                <div className="execlaw-muted small">
                                    <code>{u.user_id}</code>
                                    {u.email && <> · {u.email}</>}
                                    {u.last_login_at && (
                                        <>
                                            {" · last login "}
                                            {new Date(
                                                u.last_login_at * 1000,
                                            ).toLocaleString()}
                                        </>
                                    )}
                                </div>
                            </div>
                            {canMutate && !isMe && (
                                <div className="d-flex gap-2 align-items-center">
                                    <Button
                                        size="sm"
                                        variant="outline-secondary"
                                        onClick={() => {
                                            setResetTarget(u.user_id);
                                            setResetPassword("");
                                            setResetError(null);
                                        }}
                                        data-testid="user-reset-password"
                                    >
                                        Reset password
                                    </Button>
                                    <Button
                                        size="sm"
                                        variant="outline-danger"
                                        disabled={busyId === u.user_id}
                                        onClick={() => void onDelete(u)}
                                        data-testid="user-user-delete"
                                    >
                                        Remove
                                    </Button>
                                </div>
                            )}
                        </div>
                    );
                })
            )}

            {resetTarget && (
                <Form
                    className="mt-3 pt-2 border-top"
                    onSubmit={onSubmitReset}
                    data-testid="user-reset-form"
                >
                    <div className="execlaw-muted small mb-2">
                        Resetting password for{" "}
                        <code>{resetTarget}</code>. The user can log in
                        with the new password and rotate it from their own
                        Login page.
                    </div>
                    <ErrorBanner message={resetError} onDismiss={() => setResetError(null)} className="mb-2" />
                    <div className="row g-2">
                        <Form.Group className="col-sm-6">
                            <Form.Label className="execlaw-muted small mb-1">
                                New password
                            </Form.Label>
                            <Form.Control
                                type="password"
                                value={resetPassword}
                                onChange={(e) => setResetPassword(e.target.value)}
                                autoComplete="new-password"
                                data-testid="user-reset-password-input"
                            />
                        </Form.Group>
                    </div>
                    <div className="d-flex gap-2 mt-2">
                        <Button
                            type="submit"
                            variant="primary"
                            data-testid="user-reset-submit"
                        >
                            Set new password
                        </Button>
                        <Button
                            variant="outline-secondary"
                            onClick={() => {
                                setResetTarget(null);
                                setResetPassword("");
                                setResetError(null);
                            }}
                        >
                            Cancel
                        </Button>
                    </div>
                </Form>
            )}
        </div>
    );
}

function roleBadgeClass(role: UserRole): string {
    switch (role) {
        case "controller":
            return "is-controller";
        case "operator":
            return "is-known";
        case "viewer":
            return "is-limited";
    }
}
