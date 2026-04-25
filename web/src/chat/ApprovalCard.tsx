// Inline approval card — slides in just above the composer when the
// active thread has a pending approval (cold-contact, Rule-of-Two
// breach, sensitive tool call). Phase 6c lights up the actual verb
// buttons; this scaffold renders the card shape so the layout is
// proved out and `data-testid` hooks exist for the eventual flow tests.

interface PendingApproval {
    /** Approval id from the server. */
    approval_id: string;
    /** Human-readable summary of what's being asked. */
    summary: string;
    /** Verbs the controller can respond with. */
    verbs: string[];
}

interface Props {
    approval: PendingApproval | null;
    onRespond?: (approvalId: string, verb: string) => void;
}

export function ApprovalCard({ approval, onRespond }: Props) {
    if (!approval) return null;
    return (
        <div className="execlaw-approval-card" data-testid="approval-card">
            <div className="execlaw-approval-card__title">
                <i className="bi bi-shield-exclamation me-2" aria-hidden />
                Approval needed
            </div>
            <div className="small">{approval.summary}</div>
            <div className="d-flex gap-2 flex-wrap">
                {approval.verbs.map((verb) => (
                    <button
                        key={verb}
                        type="button"
                        className="btn btn-sm btn-outline-light"
                        data-testid={`approval-verb-${verb}`}
                        onClick={() => onRespond?.(approval.approval_id, verb)}
                    >
                        {verb}
                    </button>
                ))}
            </div>
        </div>
    );
}
