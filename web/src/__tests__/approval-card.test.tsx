import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { ApprovalCard } from "../chat/ApprovalCard";

const sample = {
    approval_id: "appr-123",
    conversation_id: "conv-x",
    sender_principal_id: "stranger-1",
    original_text: "hi can we talk",
};

describe("ApprovalCard", () => {
    it("renders nothing when no approval is supplied", () => {
        const { container } = render(<ApprovalCard approval={null} />);
        expect(container.firstChild).toBeNull();
    });

    it("renders the canonical verb buttons (snake_case wire values matching the server enum)", () => {
        render(<ApprovalCard approval={sample} />);
        // Wire values are snake_case per the server's
        // `#[serde(rename_all = "snake_case")]` on ApprovalVerb. The
        // pre-fix card sent PascalCase names that the server
        // silently rejected — this test pins the contract.
        expect(screen.getByTestId("approval-verb-trust")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-trust_limited")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-claim_as_me")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-ignore_once")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-block")).toBeInTheDocument();
    });

    it("includes the original sender id + truncated message", () => {
        render(<ApprovalCard approval={sample} />);
        expect(screen.getByTestId("approval-card")).toHaveTextContent(
            sample.sender_principal_id,
        );
        expect(screen.getByTestId("approval-card")).toHaveTextContent(
            "hi can we talk",
        );
    });

    it("clicking a verb button calls onRespond with verb + approval id", () => {
        const onRespond = vi.fn();
        render(<ApprovalCard approval={sample} onRespond={onRespond} />);
        fireEvent.click(screen.getByTestId("approval-verb-trust"));
        expect(onRespond).toHaveBeenCalledWith("appr-123", "trust");
        fireEvent.click(screen.getByTestId("approval-verb-block"));
        expect(onRespond).toHaveBeenLastCalledWith("appr-123", "block");
    });

    it("claim_as_me is a distinct verb, separate from trust", () => {
        const onRespond = vi.fn();
        render(<ApprovalCard approval={sample} onRespond={onRespond} />);
        fireEvent.click(screen.getByTestId("approval-verb-claim_as_me"));
        expect(onRespond).toHaveBeenCalledWith("appr-123", "claim_as_me");
    });

    it("disables verbs while busy", () => {
        render(<ApprovalCard approval={sample} busy />);
        for (const verb of [
            "trust",
            "trust_limited",
            "claim_as_me",
            "ignore_once",
            "block",
        ]) {
            const btn = screen.getByTestId(`approval-verb-${verb}`);
            expect((btn as HTMLButtonElement).disabled).toBe(true);
        }
    });
});
