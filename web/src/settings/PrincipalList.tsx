// Shared rendering for the Contacts and Principals pages.
//
// Both pages read the same /api/admin/principals stream — they only
// differ on which trust classes they show. ContactsPage shows the
// "address book" subset (people you'd actually message); PrincipalsPage
// shows the complement (controllers, delegated bots, blocked senders,
// anything unrecognized). Splitting the data in the SPA keeps the
// server-side principals API trust-class-agnostic.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    listPrincipals,
    revokePrincipal,
    setPrincipalTrust,
    type PrincipalSummary,
    type SettableTrustClass,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

export type PrincipalFilter = (p: PrincipalSummary) => boolean;

export interface PrincipalListProps {
    /** Test hook + container `data-testid`. */
    testId: string;
    /** Section heading shown above the list. */
    heading: string;
    /** Sentence shown when the (filtered) list is empty. */
    emptyHint: string;
    /** Optional tagline shown under the heading. */
    subhead?: string;
    /** Predicate that selects which principals belong on this page. */
    filter: PrincipalFilter;
}

export function PrincipalList(props: PrincipalListProps) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [principals, setPrincipals] = useState<PrincipalSummary[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    /// Which principal row currently has the inline edit panel open.
    /// At most one is open at a time so the page stays compact.
    const [editingId, setEditingId] = useState<string | null>(null);

    const fetchList = useCallback(async () => {
        try {
            const r = await listPrincipals(getToken);
            setPrincipals(r.principals);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void fetchList();
    }, [fetchList]);

    const onRevoke = useCallback(
        async (p: PrincipalSummary) => {
            const reason = prompt(
                `Revoke trust for ${p.display_name ?? p.id}? Future messages from them will be blocked. Optional reason:`,
                "",
            );
            if (reason === null) return; // cancelled
            setBusyId(p.id);
            try {
                await revokePrincipal(p.id, reason || undefined, getToken);
                await fetchList();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [fetchList, getToken],
    );

    const onSubmitTrust = useCallback(
        async (
            p: PrincipalSummary,
            klass: SettableTrustClass,
            allowed_topics: string[],
            reason: string,
        ) => {
            setBusyId(p.id);
            try {
                await setPrincipalTrust(
                    p.id,
                    klass,
                    { allowed_topics, reason: reason || undefined },
                    getToken,
                );
                setEditingId(null);
                await fetchList();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [fetchList, getToken],
    );

    const filtered =
        principals === null ? null : principals.filter(props.filter);

    return (
        <div data-testid={props.testId}>
            <h3 className="h6 mb-1">{props.heading}</h3>
            {props.subhead && (
                <p className="execlaw-muted small mb-3">{props.subhead}</p>
            )}

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            {filtered === null ? (
                <div className="execlaw-muted small">Loading principals…</div>
            ) : filtered.length === 0 ? (
                <div className="execlaw-muted small">{props.emptyHint}</div>
            ) : (
                filtered.map((p) => (
                    <div className="execlaw-card" key={p.id}>
                        <div className="d-flex align-items-center gap-2 mb-2">
                            <span className="execlaw-card__title flex-grow-1">
                                {p.display_name ?? p.id}
                            </span>
                            <span
                                className={
                                    "execlaw-trust-badge " +
                                    trustBadgeClass(p.trust_class)
                                }
                            >
                                {p.trust_class}
                            </span>
                        </div>
                        {p.identifiers.length > 0 && (
                            <div className="execlaw-muted small mb-2">
                                {p.identifiers
                                    .map((i) => `${i.transport}:${i.handle}`)
                                    .join(" · ")}
                            </div>
                        )}
                        <div className="execlaw-muted small mb-2">
                            <code>{p.id}</code> · first seen{" "}
                            {new Date(p.first_seen * 1000).toLocaleString()}
                        </div>
                        <div className="d-flex gap-2 align-items-center">
                            {canEditTrust(p.trust_class) && (
                                <Button
                                    size="sm"
                                    variant="outline-primary"
                                    disabled={busyId === p.id}
                                    onClick={() =>
                                        setEditingId(
                                            editingId === p.id ? null : p.id,
                                        )
                                    }
                                    data-testid="principal-edit-trust"
                                >
                                    <i className="bi bi-pencil-square me-2" aria-hidden />
                                    {editingId === p.id
                                        ? "Cancel"
                                        : "Change trust"}
                                </Button>
                            )}
                            {canRevoke(p.trust_class) && (
                                <Button
                                    size="sm"
                                    variant="outline-danger"
                                    disabled={busyId === p.id}
                                    onClick={() => void onRevoke(p)}
                                    data-testid="principal-revoke"
                                >
                                    <i className="bi bi-shield-x me-2" aria-hidden />
                                    Revoke trust
                                </Button>
                            )}
                        </div>
                        {editingId === p.id && (
                            <EditTrustPanel
                                principal={p}
                                busy={busyId === p.id}
                                onSubmit={(klass, topics, reason) =>
                                    void onSubmitTrust(p, klass, topics, reason)
                                }
                            />
                        )}
                    </div>
                ))
            )}
        </div>
    );
}

// Trust-class buckets — exported so ContactsPage and PrincipalsPage can
// keep their filters in sync (each shows the inverse of the other).

export const CONTACT_CLASSES: ReadonlySet<string> = new Set([
    "KnownTrusted",
    "KnownLimited",
    "UnknownPending",
]);

export function isContact(p: PrincipalSummary): boolean {
    return CONTACT_CLASSES.has(p.trust_class);
}

export function isPrincipalOnly(p: PrincipalSummary): boolean {
    return !CONTACT_CLASSES.has(p.trust_class);
}

function trustBadgeClass(klass: string): string {
    switch (klass) {
        case "Controller":
            return "is-controller";
        case "KnownTrusted":
        case "Delegated":
            return "is-known";
        case "KnownLimited":
            return "is-limited";
        case "UnknownPending":
            return "is-pending";
        case "Blocked":
            return "is-blocked";
        default:
            return "";
    }
}

function canRevoke(klass: string): boolean {
    return (
        klass === "KnownTrusted" ||
        klass === "KnownLimited" ||
        klass === "Delegated"
    );
}

/// "Change trust" is offered for every contact-tier class — the
/// operator can elevate UnknownPending → Trusted/Limited (the same
/// thing the cold-contact approval flow does), bump Limited up to
/// Trusted, demote Trusted back down with topic scope, or Block any
/// of them. Blocked stays editable so the operator can rehabilitate
/// a previously-blocked contact without going through revoke +
/// re-add. System tiers (Controller / Delegated) are NOT editable
/// here — they're managed via dedicated flows.
function canEditTrust(klass: string): boolean {
    return (
        klass === "KnownTrusted" ||
        klass === "KnownLimited" ||
        klass === "UnknownPending" ||
        klass === "Blocked"
    );
}

interface EditTrustPanelProps {
    principal: PrincipalSummary;
    busy: boolean;
    onSubmit: (
        klass: SettableTrustClass,
        allowed_topics: string[],
        reason: string,
    ) => void;
}

/// Inline panel rendered below a contact row when "Change trust" is
/// clicked. Radio for the target class, conditional textarea for
/// topic scope (when KnownLimited), optional reason. Stays inside
/// the card so the page doesn't shift around — and so the operator
/// always sees which contact they're editing.
function EditTrustPanel(props: EditTrustPanelProps) {
    const initial: SettableTrustClass =
        props.principal.trust_class === "KnownLimited"
            ? "KnownLimited"
            : props.principal.trust_class === "Blocked"
              ? "Blocked"
              : "KnownTrusted";
    const [klass, setKlass] = useState<SettableTrustClass>(initial);
    const [topicsText, setTopicsText] = useState("");
    const [reason, setReason] = useState("");

    const submit = () => {
        const topics =
            klass === "KnownLimited"
                ? topicsText
                      .split(",")
                      .map((s) => s.trim())
                      .filter((s) => s.length > 0)
                : [];
        props.onSubmit(klass, topics, reason);
    };

    return (
        <div
            className="execlaw-card mt-2 border border-primary-subtle"
            data-testid="principal-edit-trust-panel"
        >
            <Form
                onSubmit={(e) => {
                    e.preventDefault();
                    submit();
                }}
            >
                <div className="mb-2 small fw-semibold">Target trust class</div>
                {(
                    [
                        {
                            value: "KnownTrusted" as const,
                            label: "Known & Trusted",
                            hint: "Full access — agent treats them like a peer of the controller for the topics they bring up.",
                        },
                        {
                            value: "KnownLimited" as const,
                            label: "Known & Limited",
                            hint: "Recognised contact, scoped to a topic allowlist. Use for people you want to chat about specific things only.",
                        },
                        {
                            value: "Blocked" as const,
                            label: "Blocked",
                            hint: "Future messages from this contact get 403'd before the agent sees them.",
                        },
                    ]
                ).map((opt) => (
                    <Form.Check
                        key={opt.value}
                        type="radio"
                        id={`edit-trust-${props.principal.id}-${opt.value}`}
                        name={`edit-trust-${props.principal.id}`}
                        label={
                            <>
                                <span className="fw-semibold">{opt.label}</span>
                                <span className="execlaw-muted small ms-2">
                                    {opt.hint}
                                </span>
                            </>
                        }
                        checked={klass === opt.value}
                        onChange={() => setKlass(opt.value)}
                        data-testid={`principal-edit-trust-radio-${opt.value}`}
                    />
                ))}

                {klass === "KnownLimited" && (
                    <Form.Group className="mt-2">
                        <Form.Label className="small">
                            Allowed topics{" "}
                            <span className="execlaw-muted">
                                (comma-separated; leave blank for no
                                restrictions)
                            </span>
                        </Form.Label>
                        <Form.Control
                            type="text"
                            size="sm"
                            placeholder="scheduling, logistics"
                            value={topicsText}
                            onChange={(e) => setTopicsText(e.target.value)}
                            data-testid="principal-edit-trust-topics"
                        />
                    </Form.Group>
                )}

                <Form.Group className="mt-2">
                    <Form.Label className="small">
                        Reason{" "}
                        <span className="execlaw-muted">(optional)</span>
                    </Form.Label>
                    <Form.Control
                        type="text"
                        size="sm"
                        placeholder="why you're changing this"
                        value={reason}
                        onChange={(e) => setReason(e.target.value)}
                        data-testid="principal-edit-trust-reason"
                    />
                </Form.Group>

                <Button
                    type="submit"
                    size="sm"
                    variant="primary"
                    className="mt-2"
                    disabled={props.busy}
                    data-testid="principal-edit-trust-submit"
                >
                    <i className="bi bi-check2 me-2" aria-hidden />
                    Save trust change
                </Button>
            </Form>
        </div>
    );
}
