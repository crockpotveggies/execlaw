// Users page (Phase 7 multi-controller).
//
// Lists every user with their role badge + invite-form + delete
// button. The current `/api/admin/me` user is highlighted; deleting
// yourself is forbidden by the backend, mirrored client-side by
// disabling the button.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    deleteUser,
    inviteUser,
    listUsers,
    type UserRole,
    type UserView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

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

export function UsersPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [users, setUsers] = useState<UserView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    const [inviting, setInviting] = useState(false);
    const [inviteForm, setInviteForm] = useState<InviteFormState>(EMPTY_INVITE);
    const [inviteError, setInviteError] = useState<string | null>(null);

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
                setInviteError(
                    err instanceof Error ? err.message : String(err),
                );
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

    const meId = auth.user?.user_id ?? null;
    const meRole = auth.user?.role ?? "controller";
    const canMutate = meRole === "controller";

    return (
        <div data-testid="settings-users">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Users</h3>
                {canMutate && !inviting && (
                    <Button
                        size="sm"
                        variant="primary"
                        onClick={() => {
                            setInviting(true);
                            setInviteError(null);
                            setInviteForm(EMPTY_INVITE);
                        }}
                        data-testid="users-invite"
                    >
                        <i className="bi bi-person-plus me-2" aria-hidden />
                        Invite user
                    </Button>
                )}
            </div>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can invite or remove users.
                </div>
            )}

            {inviting && (
                <form
                    className="execlaw-card"
                    onSubmit={onInvite}
                    data-testid="users-invite-form"
                >
                    <div className="execlaw-card__title mb-3">Invite user</div>
                    {inviteError && (
                        <div className="execlaw-error-banner mb-3" role="alert">
                            {inviteError}
                        </div>
                    )}
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
                                data-testid="users-invite-username"
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
                                data-testid="users-invite-role"
                            >
                                {ROLES.map((r) => (
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
                                data-testid="users-invite-password"
                            />
                            <Form.Text className="execlaw-muted">
                                At least 8 characters. The invitee can rotate it
                                from their profile.
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
                    <div className="d-flex gap-2 mt-3">
                        <Button
                            type="submit"
                            variant="primary"
                            data-testid="users-invite-submit"
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
                </form>
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {users === null ? (
                <div className="execlaw-muted small">Loading users…</div>
            ) : (
                users.map((u) => {
                    const isMe = meId === u.user_id;
                    return (
                        <div
                            className="execlaw-card"
                            key={u.user_id}
                            data-testid="users-card"
                            data-user-id={u.user_id}
                        >
                            <div className="d-flex align-items-center gap-2 mb-1">
                                <span className="execlaw-card__title flex-grow-1">
                                    {u.display_name}
                                    <span className="execlaw-muted ms-2">
                                        @{u.username}
                                    </span>
                                    {isMe && (
                                        <span className="execlaw-trust-badge ms-2 is-controller">
                                            you
                                        </span>
                                    )}
                                </span>
                                <span
                                    className={
                                        "execlaw-trust-badge " +
                                        roleBadgeClass(u.role)
                                    }
                                >
                                    {u.role}
                                </span>
                            </div>
                            <div className="execlaw-muted small mb-2">
                                <code>{u.user_id}</code>
                                {u.email && <> · {u.email}</>} · created{" "}
                                {new Date(u.created_at * 1000).toLocaleString()}
                                {u.last_login_at && (
                                    <>
                                        {" · last login "}
                                        {new Date(
                                            u.last_login_at * 1000,
                                        ).toLocaleString()}
                                    </>
                                )}
                            </div>
                            {canMutate && !isMe && (
                                <Button
                                    size="sm"
                                    variant="outline-danger"
                                    disabled={busyId === u.user_id}
                                    onClick={() => void onDelete(u)}
                                    data-testid="users-delete"
                                >
                                    Remove
                                </Button>
                            )}
                        </div>
                    );
                })
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
