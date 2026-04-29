// Eval flags page — read /api/admin/eval/flags, optional label filter.
// Operators flag turns from the chat UI ("flag for review") and the
// agents emit eval-driven flags during turns; this page is the
// review surface.

import { useCallback, useEffect, useState } from "react";
import Form from "react-bootstrap/Form";
import { getEvalFlags, type EvalFlag } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

export function EvalFlagsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [label, setLabel] = useState("");
    const [flags, setFlags] = useState<EvalFlag[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    const fetchOnce = useCallback(async () => {
        try {
            const r = await getEvalFlags(label.trim() || undefined, getToken);
            setFlags(r.flags);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken, label]);

    useEffect(() => {
        void fetchOnce();
    }, [fetchOnce]);

    return (
        <div data-testid="settings-eval">
            <h3 className="h6 mb-3">Eval flags</h3>

            <div className="execlaw-card">
                <Form className="d-flex gap-2 align-items-end">
                    <Form.Group className="flex-grow-1">
                        <Form.Label className="execlaw-muted small mb-1">
                            Filter by label
                        </Form.Label>
                        <Form.Control
                            value={label}
                            onChange={(e) => setLabel(e.target.value)}
                            placeholder="any"
                        />
                    </Form.Group>
                </Form>
            </div>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            <div className="execlaw-card">
                {flags === null ? (
                    <div className="execlaw-muted small">Loading…</div>
                ) : flags.length === 0 ? (
                    <div className="execlaw-muted small">
                        No eval flags match the current filter. Operators flag
                        turns from the chat UI; the eval harness can emit
                        flags during run.
                    </div>
                ) : (
                    flags.map((f) => (
                        <div className="execlaw-card__row" key={f.id}>
                            <div>
                                <strong>{f.label}</strong>
                                <div className="execlaw-muted small">
                                    {f.conversation_id} · seq {f.seq} ·{" "}
                                    {new Date(f.flagged_at * 1000).toLocaleString()}
                                </div>
                                {f.notes && (
                                    <div className="small mt-1">{f.notes}</div>
                                )}
                            </div>
                        </div>
                    ))
                )}
            </div>
        </div>
    );
}
