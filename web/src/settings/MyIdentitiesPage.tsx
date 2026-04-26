// Settings → My identities (Phase 9.3, §7.1).
//
// v1 surface: list, add, and remove transport-scoped handles attached
// to the controller's principal. Inbound transport messages from any
// of these handles resolve to the controller and skip the cold-contact
// ladder entirely.
//
// Verification challenges (email magic-link, SMS code, Signal device-
// link) are deferred to Phase 11 when transport plugins ship — v1
// trusts the operator's assertion at face value, which is acceptable
// because the controller already has full system access.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    addMyIdentifier,
    deleteMyIdentifier,
    listMyIdentifiers,
    type IdentifierView,
    type MyIdentitiesResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

const TRANSPORT_HINTS = [
    { value: "signal", label: "Signal", placeholder: "+15551234" },
    { value: "email", label: "Email", placeholder: "you@example.com" },
    { value: "sms", label: "SMS", placeholder: "+15551234" },
    { value: "voice", label: "Voice", placeholder: "+15551234" },
    { value: "web", label: "Web session", placeholder: "session-id" },
];

export function MyIdentitiesPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [data, setData] = useState<MyIdentitiesResponse | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [transport, setTransport] = useState("signal");
    const [handle, setHandle] = useState("");
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await listMyIdentifiers(getToken);
            setData(r);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onAdd = useCallback(async () => {
        if (handle.trim() === "") return;
        setBusy(true);
        try {
            const r = await addMyIdentifier(transport, handle.trim(), getToken);
            setData(r);
            setHandle("");
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [transport, handle, getToken]);

    const onDelete = useCallback(
        async (id: IdentifierView) => {
            if (
                !confirm(
                    `Remove ${id.transport}:${id.handle} from your controller identity? Inbound messages from this handle will fall through the cold-contact ladder again.`,
                )
            )
                return;
            setBusy(true);
            try {
                const r = await deleteMyIdentifier(
                    id.transport,
                    id.handle,
                    getToken,
                );
                setData(r);
                setError(null);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [getToken],
    );

    const placeholder =
        TRANSPORT_HINTS.find((t) => t.value === transport)?.placeholder ??
        "handle";

    return (
        <div data-testid="settings-my-identities">
            <h3 className="h6 mb-1">My identities</h3>
            <p className="execlaw-muted small mb-3">
                Transport-scoped handles that should resolve to you (the
                controller). When a message lands on any of these handles
                it bypasses the cold-contact ladder and is treated as
                yours.
                <br />
                <span className="execlaw-muted">
                    v1 takes assertions at face value. Verification
                    challenges (email magic-link, SMS code, Signal
                    device-link) ship with the corresponding transport
                    plugins.
                </span>
            </p>

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            <div
                className="execlaw-card mb-3"
                data-testid="my-identities-add-card"
            >
                <div className="execlaw-card__title mb-2">
                    Add an identifier
                </div>
                <div className="d-flex gap-2 align-items-end">
                    <Form.Group className="flex-grow-0" controlId="ident-transport">
                        <Form.Label className="small mb-1">Transport</Form.Label>
                        <Form.Select
                            value={transport}
                            onChange={(e) => setTransport(e.target.value)}
                            data-testid="my-identities-transport"
                            style={{ minWidth: "10rem" }}
                        >
                            {TRANSPORT_HINTS.map((t) => (
                                <option key={t.value} value={t.value}>
                                    {t.label}
                                </option>
                            ))}
                        </Form.Select>
                    </Form.Group>
                    <Form.Group className="flex-grow-1" controlId="ident-handle">
                        <Form.Label className="small mb-1">Handle</Form.Label>
                        <Form.Control
                            type="text"
                            value={handle}
                            onChange={(e) => setHandle(e.target.value)}
                            placeholder={placeholder}
                            data-testid="my-identities-handle"
                        />
                    </Form.Group>
                    <Button
                        variant="primary"
                        size="sm"
                        onClick={() => void onAdd()}
                        disabled={busy || handle.trim() === ""}
                        data-testid="my-identities-add"
                    >
                        Add
                    </Button>
                </div>
            </div>

            <div className="execlaw-card">
                <div className="execlaw-card__title mb-2">
                    Current identifiers
                </div>
                {data === null ? (
                    <div className="execlaw-muted small">Loading…</div>
                ) : data.identifiers.length === 0 ? (
                    <div
                        className="execlaw-muted small"
                        data-testid="my-identities-empty"
                    >
                        Nothing yet. Add your Signal, email, or other
                        transport handles above.
                    </div>
                ) : (
                    <ul className="list-unstyled mb-0">
                        {data.identifiers.map((id) => (
                            <li
                                key={`${id.transport}:${id.handle}`}
                                className="d-flex align-items-baseline gap-2 mb-2"
                                data-testid="my-identities-row"
                            >
                                <span className="execlaw-trust-badge is-known">
                                    {id.transport}
                                </span>
                                <code className="flex-grow-1">
                                    {id.handle}
                                </code>
                                <Button
                                    size="sm"
                                    variant="outline-danger"
                                    onClick={() => void onDelete(id)}
                                    disabled={busy}
                                    data-testid="my-identities-delete"
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
