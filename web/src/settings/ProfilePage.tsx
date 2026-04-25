// Settings → Profile (Phase 7e WebAuthn credential management).
//
// One section today: Passkeys. Lists the operator's registered
// credentials, lets them register a new one (Add passkey), and remove
// existing ones. Each row shows the operator-supplied label, the
// short credential id, and the last-used / created timestamps.
//
// Adding a passkey kicks off the WebAuthn registration ceremony:
//   1. POST /api/admin/webauthn/register/begin → server returns
//      challenge JSON.
//   2. navigator.credentials.create(options) → browser prompts the
//      user, returns a PublicKeyCredential.
//   3. POST /api/admin/webauthn/register/finish → server validates
//      and persists.
//
// We DO NOT touch passwords from this page — password rotation
// already lives elsewhere. Operators who want to disable webauthn
// remove every credential; the login route falls back to password-
// only the moment count_for_user reaches zero.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    beginWebauthnRegistration,
    deleteWebauthnCredential,
    finishWebauthnRegistration,
    listWebauthnCredentials,
    type WebauthnCredentialView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import {
    coerceCreationOptions,
    serializeCredential,
} from "../auth/webauthn";

export function ProfilePage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [creds, setCreds] = useState<WebauthnCredentialView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [label, setLabel] = useState("");

    const refresh = useCallback(async () => {
        try {
            const r = await listWebauthnCredentials(getToken);
            setCreds(r.credentials);
            setError(null);
        } catch (e) {
            // 503 means the server was built without webauthn support.
            // Surface it as an empty list with a hint, not a hard error.
            if (
                e instanceof Error &&
                /webauthn[_ ]unconfigured/i.test(e.message)
            ) {
                setCreds([]);
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
            setError(e instanceof Error ? e.message : String(e));
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
        <div data-testid="settings-profile">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Profile</h3>
            </div>

            <div className="execlaw-card mb-3">
                <div className="execlaw-card__title mb-2">
                    <i className="bi bi-key me-2" aria-hidden />
                    Passkeys (WebAuthn)
                </div>
                <p className="execlaw-muted small mb-3">
                    Register a security key or platform authenticator as a
                    second factor. Sign-in still requires your password —
                    the passkey is checked after.
                </p>

                {error && (
                    <div className="execlaw-error-banner mb-3" role="alert">
                        {error}
                    </div>
                )}

                <div className="d-flex gap-2 mb-3">
                    <Form.Control
                        value={label}
                        onChange={(e) => setLabel(e.target.value)}
                        placeholder="Label this passkey (e.g. YubiKey 5C)"
                        disabled={busy}
                        data-testid="profile-passkey-label"
                    />
                    <Button
                        variant="primary"
                        disabled={busy || label.trim().length === 0}
                        onClick={() => void onAdd()}
                        data-testid="profile-passkey-add"
                    >
                        Add passkey
                    </Button>
                </div>

                {creds === null ? (
                    <div className="execlaw-muted small">Loading…</div>
                ) : creds.length === 0 ? (
                    <div className="execlaw-muted small">
                        No passkeys registered. Sign-in will require only
                        your password.
                    </div>
                ) : (
                    <ul className="list-unstyled m-0">
                        {creds.map((c) => (
                            <li
                                key={c.credential_id}
                                className="d-flex align-items-center py-2 border-top"
                                data-testid="profile-passkey-row"
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
                                    data-testid="profile-passkey-remove"
                                >
                                    Remove
                                </Button>
                            </li>
                        ))}
                    </ul>
                )}
            </div>
        </div>
    );
}
