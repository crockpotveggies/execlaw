// Audit page — newest-first feed of `config_audit` rows. Empty until
// config-write routes start logging entries (Phase 7 deployment
// editor onward).

import { useEffect, useState } from "react";
import { getAuditEntries, type AuditEntry } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

export function AuditPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [entries, setEntries] = useState<AuditEntry[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await getAuditEntries(undefined, 200, getToken);
                if (!cancelled) setEntries(r.entries);
            } catch (e) {
                if (!cancelled)
                    setError(e instanceof Error ? e.message : String(e));
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    return (
        <div data-testid="settings-audit">
            <h3 className="h6 mb-3">Audit log</h3>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            <div className="execlaw-card">
                {entries === null ? (
                    <div className="execlaw-muted small">Loading…</div>
                ) : entries.length === 0 ? (
                    <div className="execlaw-muted small">
                        No audit entries yet. Mutations to
                        <code className="ms-1">config_*</code> tables will
                        appear here once the deployment editor + other Phase-7
                        write routes begin emitting.
                    </div>
                ) : (
                    entries.map((e) => (
                        <details className="execlaw-card__row" key={e.id}>
                            <summary>
                                <strong>{e.actor}</strong> ·{" "}
                                <code>{e.table_name}</code>:{" "}
                                <code>{e.row_id}</code>
                                <span className="execlaw-muted small ms-2">
                                    {new Date(e.ts * 1000).toLocaleString()}
                                </span>
                            </summary>
                            <div className="row mt-2">
                                <div className="col-sm-6">
                                    <div className="execlaw-muted small">old</div>
                                    <pre className="small mb-0">
                                        {JSON.stringify(e.old_json ?? null, null, 2)}
                                    </pre>
                                </div>
                                <div className="col-sm-6">
                                    <div className="execlaw-muted small">new</div>
                                    <pre className="small mb-0">
                                        {JSON.stringify(e.new_json ?? null, null, 2)}
                                    </pre>
                                </div>
                            </div>
                        </details>
                    ))
                )}
            </div>
        </div>
    );
}
