// Principals page — list every participant the system has seen, with
// trust-class badges and a revoke action for trusted contacts.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import {
    listPrincipals,
    revokePrincipal,
    type PrincipalSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

export function PrincipalsPage() {
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

    return (
        <div data-testid="settings-principals">
            <h3 className="h6 mb-3">Principals</h3>

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {principals === null ? (
                <div className="execlaw-muted small">Loading principals…</div>
            ) : principals.length === 0 ? (
                <div className="execlaw-muted small">
                    No principals on file yet. They'll appear here as cold
                    contacts arrive and as the controller approves them.
                </div>
            ) : (
                principals.map((p) => (
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
