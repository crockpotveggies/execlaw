// Inline approval card — slides in above the composer when the
// active thread has a pending cold-contact approval. Wire values
// match the server's ApprovalVerb enum (snake_case); see
// `crates/policy/src/sideband.rs`.

import Button from "react-bootstrap/Button";
import type { ApprovalVerb, PendingApprovalSummary } from "../api/endpoints";

const VERBS: ReadonlyArray<{
    verb: ApprovalVerb;
    label: string;
    icon: string;
    variant: string;
    title: string;
}> = [
    {
        verb: "trust",
        label: "Trust",
        icon: "bi-shield-check",
        variant: "outline-success",
        title: "Admit as KnownTrusted: full safe-tools + memory access",
    },
    {
        verb: "trust_limited",
        label: "Limited",
        icon: "bi-shield-shaded",
        variant: "outline-warning",
        title: "Admit as KnownLimited: agent can reply on this transport only",
    },
    {
        verb: "claim_as_me",
        label: "This is me",
        icon: "bi-person-check",
        variant: "outline-primary",
        title: "Adds this handle to your My identities so future inbound resolves to you",
    },
    {
        verb: "ignore_once",
        label: "Ignore once",
        icon: "bi-shield-slash",
        variant: "outline-secondary",
        title: "Drop this message; re-prompt on the next inbound from this handle",
    },
    {
        verb: "block",
        label: "Block",
        icon: "bi-shield-x",
        variant: "outline-danger",
        title: "Block universally — future inbound silently audit-logged",
    },
];

interface Props {
    approval: PendingApprovalSummary | null;
    busy?: boolean;
    onRespond?: (approvalId: string, verb: ApprovalVerb) => void;
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
                        title={v.title}
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
