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
import {
    listPrincipals,
    revokePrincipal,
    type PrincipalSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

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
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [principals, setPrincipals] = useState<PrincipalSummary[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);

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

    const filtered =
        principals === null ? null : principals.filter(props.filter);

    return (
        <div data-testid={props.testId}>
            <h3 className="h6 mb-1">{props.heading}</h3>
            {props.subhead && (
                <p className="execlaw-muted small mb-3">{props.subhead}</p>
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

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
