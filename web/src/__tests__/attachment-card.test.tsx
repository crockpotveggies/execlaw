// Tests for the AttachmentCard renderer.
//
// The card is the web-channel equivalent of channel-plugin
// `send_attachment` — agent emits one to deliver a file (e.g. a
// research PDF) inline. Renders as a compact chip with filename +
// mime + size + a Download button that hits
// `/api/attachments/<attachment_id>`.

import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { AttachmentCard } from "../cards/AttachmentCard";
import { getCardRenderer } from "../cards/CardRenderer";
import type { Card } from "../cards/types";

function makeAttachmentCard(extras: Partial<Card> = {}): Card {
    return {
        card_id: "att-card-1",
        conversation_id: "conv-1",
        kind: "attachment",
        state: "Completed",
        title: "report.pdf",
        summary: "report.pdf (application/pdf)",
        progress: null,
        phase: null,
        details: {
            attachment_id: "att-9",
            filename: "report.pdf",
            mime_type: "application/pdf",
            byte_size: 245_678,
            download_url: "/api/attachments/att-9",
            caption: null,
        },
        actions: [{ kind: "OpenDetail", href: "/api/attachments/att-9" }],
        error: null,
        opened_at: 1,
        updated_at: 1,
        event_seq: null,
        attachment_id: "att-9",
        ...extras,
    };
}

describe("AttachmentCard renderer", () => {
    it("registers under the `attachment` kind", () => {
        // Side-effect import already ran — getCardRenderer must
        // return the dedicated component, not the LongRunningTask
        // fallback.
        const Rendered = getCardRenderer("attachment");
        expect(Rendered).toBe(AttachmentCard);
    });

    it("renders the filename, mime, and human-readable size", () => {
        render(<AttachmentCard card={makeAttachmentCard()} />);
        const chip = screen.getByTestId("card-attachment");
        expect(chip).toBeTruthy();
        expect(screen.getByTestId("card-attachment-filename").textContent).toBe(
            "report.pdf",
        );
        // 245678 / 1024 = 239.92 KB → formatted as "240 KB" (no
        // decimals since >= 10).
        const size = screen.getByTestId("card-attachment-size").textContent;
        expect(size).toMatch(/KB|MB/);
    });

    it("renders a Download button that links to the server URL with `download` attribute", () => {
        render(<AttachmentCard card={makeAttachmentCard()} />);
        const link = screen.getByTestId(
            "card-attachment-download",
        ) as HTMLAnchorElement;
        expect(link.getAttribute("href")).toBe("/api/attachments/att-9");
        expect(link.getAttribute("download")).toBe("report.pdf");
    });

    it("falls back to deriving the URL from attachment_id if download_url missing", () => {
        // Defensive: legacy events that don't include `download_url`
        // should still produce a working button.
        const card = makeAttachmentCard({
            details: {
                attachment_id: "att-only",
                filename: "thing.pdf",
                mime_type: "application/pdf",
            },
        });
        render(<AttachmentCard card={card} />);
        const link = screen.getByTestId(
            "card-attachment-download",
        ) as HTMLAnchorElement;
        expect(link.getAttribute("href")).toBe("/api/attachments/att-only");
    });

    it("shows a caption above the chip when set", () => {
        const card = makeAttachmentCard({
            details: {
                attachment_id: "att-9",
                filename: "report.pdf",
                mime_type: "application/pdf",
                caption: "Final research report on ground covers",
                download_url: "/api/attachments/att-9",
            },
        });
        render(<AttachmentCard card={card} />);
        expect(
            screen.getByTestId("card-attachment-caption").textContent,
        ).toContain("ground covers");
    });

    it("hides the caption block when caption is null/empty", () => {
        render(<AttachmentCard card={makeAttachmentCard()} />);
        expect(screen.queryByTestId("card-attachment-caption")).toBeNull();
    });

    it("omits the size pill when byte_size is unknown", () => {
        const card = makeAttachmentCard({
            details: {
                attachment_id: "att-9",
                filename: "report.pdf",
                mime_type: "application/pdf",
                download_url: "/api/attachments/att-9",
                byte_size: null,
            },
        });
        render(<AttachmentCard card={card} />);
        expect(screen.queryByTestId("card-attachment-size")).toBeNull();
    });

    it("falls back to the card title if no filename in details", () => {
        const card = makeAttachmentCard({
            title: "fallback.bin",
            details: {
                attachment_id: "att-9",
                mime_type: "application/octet-stream",
                download_url: "/api/attachments/att-9",
            },
        });
        render(<AttachmentCard card={card} />);
        expect(screen.getByTestId("card-attachment-filename").textContent).toBe(
            "fallback.bin",
        );
    });
});
