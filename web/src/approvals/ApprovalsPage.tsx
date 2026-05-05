// Settings-style page body listing every pending cold-contact
// approval. Each row is a card with the message preview + action
// buttons. Polls `/api/admin/approvals` every 4s while mounted so a
// freshly-arrived approval surfaces without a manual refresh.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import {
    listPendingApprovals,
    respondApproval,
    type ApprovalVerb,
    type PendingApprovalSummary,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

const POLL_INTERVAL_MS = 4_000;

interface ActionButton {
    verb: ApprovalVerb;
    label: string;
    icon: string;
    variant: string;
    title: string;
}

const ACTIONS: ReadonlyArray<ActionButton> = [
    {
        verb: "trust",
        label: "Trust",
        icon: "bi-shield-check",
        variant: "outline-success",
        title: "Admit as KnownTrusted: full safe-tools + memory access. Agent replies to the queued message.",
    },
    {
        verb: "trust_limited",
        label: "Limited",
        icon: "bi-shield-shaded",
        variant: "outline-warning",
        title: "Admit as KnownLimited: agent can reply on this transport only. Agent replies to the queued message.",
    },
    {
        verb: "claim_as_me",
        label: "This is me",
        icon: "bi-person-check",
        variant: "outline-primary",
        title: "Adds this handle to your My identities. Future inbound from this number resolves to you. Replays the queued message as a Controller turn.",
    },
    {
        verb: "ignore_once",
        label: "Ignore once",
        icon: "bi-shield-slash",
        variant: "outline-secondary",
        title: "Drop this message; re-prompt on the next inbound from this handle.",
    },
    {
        verb: "block",
        label: "Block",
        icon: "bi-shield-x",
        variant: "outline-danger",
        title: "Block universally — future inbound silently audit-logged.",
    },
];

export function ApprovalsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [approvals, setApprovals] = useState<PendingApprovalSummary[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyApproval, setBusyApproval] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listPendingApprovals(getToken);
            setApprovals(r.approvals);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
        const id = window.setInterval(() => {
            void refresh();
        }, POLL_INTERVAL_MS);
        return () => window.clearInterval(id);
    }, [refresh]);

    const onRespond = useCallback(
        async (approvalId: string, verb: ApprovalVerb) => {
            setBusyApproval(approvalId);
            try {
                await respondApproval(approvalId, { verb }, getToken);
                // Optimistic: drop this approval from the list. The
                // next poll re-confirms.
                setApprovals((prev) =>
                    prev ? prev.filter((a) => a.approval_id !== approvalId) : prev,
                );
                setError(null);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyApproval(null);
                // Re-fetch so any server-side changes (e.g. claim_as_me
                // merging multiple approvals into one) are reflected.
                void refresh();
            }
        },
        [getToken, refresh],
    );

    if (approvals === null) {
        return (
            <div data-testid="approvals-page-body">
                <ErrorBanner
                    message={error}
                    onDismiss={() => setError(null)}
                    className="mb-3"
                />
                <div className="execlaw-muted small">Loading approvals…</div>
            </div>
        );
    }

    return (
        <div data-testid="approvals-page-body">
            <p className="execlaw-muted small mb-3">
                Cold contacts waiting on a trust decision. Approving an
                entry replays the queued first message through the agent
                — you don&apos;t need to ask the contact to re-send.
            </p>

            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            {approvals.length === 0 ? (
                <div
                    className="execlaw-card text-center execlaw-muted small"
                    data-testid="approvals-empty"
                    style={{ padding: "2rem" }}
                >
                    <i
                        className="bi bi-shield-check d-block mb-2"
                        style={{ fontSize: "1.5rem" }}
                        aria-hidden
                    />
                    No pending approvals. New cold contacts will appear here
                    when they message the agent.
                </div>
            ) : (
                <ul className="list-unstyled mb-0">
                    {approvals.map((a) => (
                        <li
                            key={a.approval_id}
                            className="execlaw-card mb-3"
                            data-testid="approval-row"
                        >
                            <div className="d-flex align-items-start gap-2 mb-2">
                                <i
                                    className="bi bi-shield-exclamation execlaw-muted"
                                    style={{ fontSize: "1.25rem" }}
                                    aria-hidden
                                />
                                <div className="flex-grow-1">
                                    <div className="execlaw-muted small mb-1">
                                        Sender: <code>{a.sender_principal_id}</code>
                                    </div>
                                    <div
                                        style={{
                                            background: "rgba(0,0,0,0.05)",
                                            borderRadius: "0.5rem",
                                            padding: "0.5rem 0.75rem",
                                            fontStyle: "italic",
                                        }}
                                        data-testid="approval-row-text"
                                    >
                                        &ldquo;{truncate(a.original_text, 280)}&rdquo;
                                    </div>
                                </div>
                            </div>
                            <div className="d-flex gap-2 flex-wrap">
                                {ACTIONS.map((act) => (
                                    <Button
                                        key={act.verb}
                                        size="sm"
                                        variant={act.variant}
                                        disabled={busyApproval !== null}
                                        title={act.title}
                                        onClick={() =>
                                            void onRespond(a.approval_id, act.verb)
                                        }
                                        data-testid={`approval-row-verb-${act.verb}`}
                                    >
                                        <i
                                            className={`bi ${act.icon} me-2`}
                                            aria-hidden
                                        />
                                        {act.label}
                                    </Button>
                                ))}
                            </div>
                        </li>
                    ))}
                </ul>
            )}
        </div>
    );
}

function truncate(s: string, n: number): string {
    if (s.length <= n) return s;
    return s.slice(0, n - 1) + "…";
}
