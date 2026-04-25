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

    it("renders the four canonical verb buttons", () => {
        render(<ApprovalCard approval={sample} />);
        expect(screen.getByTestId("approval-verb-Trust")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-TrustLimited")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-TrustOnce")).toBeInTheDocument();
        expect(screen.getByTestId("approval-verb-Block")).toBeInTheDocument();
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
        fireEvent.click(screen.getByTestId("approval-verb-Trust"));
        expect(onRespond).toHaveBeenCalledWith("appr-123", "Trust");
        fireEvent.click(screen.getByTestId("approval-verb-Block"));
        expect(onRespond).toHaveBeenLastCalledWith("appr-123", "Block");
    });

    it("disables verbs while busy", () => {
        render(<ApprovalCard approval={sample} busy />);
        for (const verb of ["Trust", "TrustLimited", "TrustOnce", "Block"]) {
            const btn = screen.getByTestId(`approval-verb-${verb}`);
            expect((btn as HTMLButtonElement).disabled).toBe(true);
        }
    });
});
