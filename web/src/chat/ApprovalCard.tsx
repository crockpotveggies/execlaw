// Inline approval card — slides in just above the composer when the
// active thread has a pending approval (cold-contact). Verbs map
// directly to the server's ApprovalVerb enum: Trust / TrustLimited /
// Block / TrustOnce.

import Button from "react-bootstrap/Button";
import type { PendingApprovalSummary } from "../api/endpoints";

const VERBS: ReadonlyArray<{
    verb: "Trust" | "TrustLimited" | "Block" | "TrustOnce";
    label: string;
    icon: string;
    variant: string;
}> = [
    { verb: "Trust", label: "Trust", icon: "bi-shield-check", variant: "outline-success" },
    {
        verb: "TrustLimited",
        label: "Limited",
        icon: "bi-shield-shaded",
        variant: "outline-warning",
    },
    {
        verb: "TrustOnce",
        label: "Trust once",
        icon: "bi-shield-slash",
        variant: "outline-secondary",
    },
    { verb: "Block", label: "Block", icon: "bi-shield-x", variant: "outline-danger" },
];

interface Props {
    approval: PendingApprovalSummary | null;
    busy?: boolean;
    onRespond?: (
        approvalId: string,
        verb: "Trust" | "TrustLimited" | "Block" | "TrustOnce",
    ) => void;
}

export function ApprovalCard({ approval, busy, onRespond }: Props) {
    if (!approval) return null;
    return (
        <div className="execlaw-approval-card" data-testid="approval-card">
            <div className="execlaw-approval-card__title">
                <i className="bi bi-shield-exclamation me-2" aria-hidden />
                Approval needed
            </div>
            <div className="small">
                <strong>Cold contact</strong> sent:{" "}
                <span className="execlaw-muted">"{truncate(approval.original_text, 200)}"</span>
            </div>
            <div className="execlaw-muted small">
                Sender: <code>{approval.sender_principal_id}</code>
            </div>
            <div className="d-flex gap-2 flex-wrap">
                {VERBS.map((v) => (
                    <Button
                        key={v.verb}
                        size="sm"
                        variant={v.variant}
                        disabled={busy}
                        onClick={() => onRespond?.(approval.approval_id, v.verb)}
                        data-testid={`approval-verb-${v.verb}`}
                    >
                        <i className={`bi ${v.icon} me-2`} aria-hidden />
                        {v.label}
                    </Button>
                ))}
            </div>
        </div>
    );
}

function truncate(s: string, n: number): string {
    if (s.length <= n) return s;
    return s.slice(0, n - 1) + "…";
}
