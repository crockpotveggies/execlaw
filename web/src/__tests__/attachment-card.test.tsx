// Tests for the AttachmentCard renderer.
//
// The card is the web-channel equivalent of channel-plugin
// `send_attachment` — agent emits one to deliver a file (e.g. a
// research PDF) inline. Renders as a compact chip with filename +
// mime + size + a Download button that hits
// `/api/attachments/<attachment_id>` via a server-signed URL.

import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { AttachmentCard } from "../cards/AttachmentCard";
import { AuthContext } from "../auth/AuthContext";
import { getCardRenderer } from "../cards/CardRenderer";
import type { Card } from "../cards/types";

// Mock the signed-URL helper. Tests assert that the renderer calls
// it with the correct `path` and renders the returned URL into the
// download link's `href`. The 2026-05-19 security fix replaced the
// pre-fix `?access_token=<jwt>` query-string pattern with this
// server-mediated flow.
vi.mock("../api/signedDownloadUrl", () => ({
    signDownloadUrl: vi.fn(
        async (path: string) =>
            `${path}?exp=9999999999&user=u-test&sig=deadbeefcafebabe`,
    ),
}));

import { signDownloadUrl } from "../api/signedDownloadUrl";

afterEach(() => {
    vi.mocked(signDownloadUrl).mockClear();
});

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

function fakeAuth(): React.ContextType<typeof AuthContext> {
    return {
        getAccessToken: () => "header.payload.signature",
    } as unknown as React.ContextType<typeof AuthContext>;
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

    /// 2026-05-19 — clicking Download must hit a SIGNED URL, not a
    /// raw JWT in the query string. The renderer issues
    /// `POST /api/downloads/sign` on mount with the attachment
    /// path, then puts the returned URL in the link's href.
    it("renders Download button with the server-signed URL", async () => {
        render(
            <AuthContext.Provider value={fakeAuth()}>
                <AttachmentCard card={makeAttachmentCard()} />
            </AuthContext.Provider>,
        );
        const link = (await screen.findByTestId(
            "card-attachment-download",
        )) as HTMLAnchorElement;
        await waitFor(() => {
            expect(link.getAttribute("href")).toBe(
                "/api/attachments/att-9?exp=9999999999&user=u-test&sig=deadbeefcafebabe",
            );
        });
        expect(signDownloadUrl).toHaveBeenCalledWith(
            "/api/attachments/att-9",
            expect.any(Function),
        );
        expect(link.getAttribute("download")).toBe("report.pdf");
    });

    it("never emits a raw `?access_token=` JWT in the download href", async () => {
        render(
            <AuthContext.Provider value={fakeAuth()}>
                <AttachmentCard card={makeAttachmentCard()} />
            </AuthContext.Provider>,
        );
        const link = (await screen.findByTestId(
            "card-attachment-download",
        )) as HTMLAnchorElement;
        await waitFor(() => {
            expect(link.getAttribute("href")).toBeTruthy();
        });
        // The audit's hard rule: a full-access JWT must never travel
        // through the URL. Belt-and-suspenders regression bar.
        expect(link.getAttribute("href") ?? "").not.toMatch(/access_token=/);
        expect(link.getAttribute("href") ?? "").not.toContain(
            "header.payload.signature",
        );
    });

    it("hides the Download button when no AuthProvider is mounted (cannot sign)", () => {
        // Production always has a provider; some unit tests don't.
        // Without auth there's no `getAccessToken`, so the sign call
        // can't run — the chip renders without a button rather than
        // emitting an unauthenticated href.
        render(<AttachmentCard card={makeAttachmentCard()} />);
        expect(screen.queryByTestId("card-attachment-download")).toBeNull();
        expect(signDownloadUrl).not.toHaveBeenCalled();
    });

    it("falls back to deriving the URL from attachment_id if download_url missing", async () => {
        // Defensive: legacy events without `download_url` still
        // produce a working button.
        const card = makeAttachmentCard({
            details: {
                attachment_id: "att-only",
                filename: "thing.pdf",
                mime_type: "application/pdf",
            },
        });
        render(
            <AuthContext.Provider value={fakeAuth()}>
                <AttachmentCard card={card} />
            </AuthContext.Provider>,
        );
        await waitFor(() => {
            expect(signDownloadUrl).toHaveBeenCalledWith(
                "/api/attachments/att-only",
                expect.any(Function),
            );
        });
        const link = (await screen.findByTestId(
            "card-attachment-download",
        )) as HTMLAnchorElement;
        expect(link.getAttribute("href") ?? "").toContain(
            "/api/attachments/att-only",
        );
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
