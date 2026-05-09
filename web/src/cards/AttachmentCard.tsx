// Per-kind renderer for `kind: attachment` cards.
//
// Rendered as a compact inline chip rather than a progress card —
// the agent emits these via `send_attachment` to deliver a file
// (e.g. a deep-research PDF) into the chat. The chip carries the
// filename + mime type + size and a Download button that hits
// `/api/attachments/<attachment_id>` (the streaming endpoint
// added alongside this renderer).
//
// Symmetric to channel-plugin `send_attachment` impls for SMS /
// email — those transport the file out-of-band; the web channel
// surfaces it as this chip.

import { useContext } from "react";
import { AuthContext } from "../auth/AuthContext";
import { registerCardRenderer, type CardRendererProps } from "./CardRenderer";

interface AttachmentDetails {
    attachment_id?: string;
    filename?: string;
    mime_type?: string;
    byte_size?: number | null;
    download_url?: string;
    caption?: string | null;
}

function readDetails(raw: unknown): AttachmentDetails | null {
    if (!raw || typeof raw !== "object") return null;
    return raw as AttachmentDetails;
}

function formatBytes(n: number | null | undefined): string | null {
    if (n == null || !Number.isFinite(n) || n < 0) return null;
    if (n < 1024) return `${n} B`;
    const kb = n / 1024;
    if (kb < 1024) return `${kb.toFixed(kb < 10 ? 1 : 0)} KB`;
    const mb = kb / 1024;
    if (mb < 1024) return `${mb.toFixed(mb < 10 ? 1 : 0)} MB`;
    const gb = mb / 1024;
    return `${gb.toFixed(gb < 10 ? 1 : 0)} GB`;
}

function iconForMime(mime?: string): string {
    if (!mime) return "bi-file-earmark";
    if (mime === "application/pdf") return "bi-file-earmark-pdf";
    if (mime.startsWith("image/")) return "bi-file-earmark-image";
    if (mime.startsWith("text/markdown")) return "bi-file-earmark-text";
    if (mime.startsWith("text/")) return "bi-file-earmark-text";
    if (mime === "application/json") return "bi-file-earmark-code";
    if (mime.startsWith("audio/")) return "bi-file-earmark-music";
    if (mime.startsWith("video/")) return "bi-file-earmark-play";
    if (
        mime ===
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" ||
        mime === "application/vnd.ms-excel" ||
        mime === "text/csv"
    )
        return "bi-file-earmark-spreadsheet";
    return "bi-file-earmark";
}

export function AttachmentCard({ card }: CardRendererProps) {
    // Read AuthContext directly (instead of via the standard
    // `useAuth()` hook) so this renderer doesn't throw in test
    // environments that mount cards without an AuthProvider.
    // Production always has the provider — `null` only happens
    // in unit tests that focus on the chip's visual behaviour.
    const auth = useContext(AuthContext);
    const details = readDetails(card.details);
    // Prefer the URL the server emitted (`/api/attachments/<id>`).
    // Fallback constructs it from `attachment_id` so older cards
    // that pre-date the field still render a working button.
    const baseUrl =
        details?.download_url ??
        (details?.attachment_id
            ? `/api/attachments/${details.attachment_id}`
            : null);
    // 2026-05-04: append `?access_token=<jwt>` so the
    // browser's link navigation authenticates. Browsers don't
    // carry the Authorization header on `<a download href>`
    // requests, so without this the streaming-attachment endpoint
    // returns 401 even though the operator is signed in. The
    // server's AuthedUser extractor reads `?access_token=…` as a
    // fallback to the Bearer header for exactly this case.
    //
    // Token comes from accessTokenRef via getAccessToken — a
    // straight ref read, no React re-render dependency. The href
    // updates on the next render after a token refresh; if a
    // user clicks during the brief window between expiry and
    // refresh the request 401s and the chip surfaces the error
    // (acceptable — same shape as any other expired-token
    // request).
    const token = auth?.getAccessToken() ?? null;
    const url = baseUrl
        ? token
            ? `${baseUrl}${baseUrl.includes("?") ? "&" : "?"}access_token=${encodeURIComponent(
                  token,
              )}`
            : baseUrl
        : null;
    const filename = details?.filename ?? card.title ?? "attachment";
    const sizeLabel = formatBytes(details?.byte_size);
    const caption = details?.caption ?? null;
    const mime = details?.mime_type;

    return (
        <div
            className="execlaw-card-attachment"
            data-testid="card-attachment"
            data-card-id={card.card_id}
        >
            {caption && (
                <div
                    className="execlaw-card-attachment__caption"
                    data-testid="card-attachment-caption"
                >
                    {caption}
                </div>
            )}
            <div className="execlaw-card-attachment__chip">
                <i
                    className={`bi ${iconForMime(mime)} execlaw-card-attachment__icon`}
                    aria-hidden
                />
                <div className="execlaw-card-attachment__meta">
                    <div
                        className="execlaw-card-attachment__filename"
                        data-testid="card-attachment-filename"
                    >
                        {filename}
                    </div>
                    <div className="execlaw-card-attachment__sub execlaw-muted small">
                        {mime ?? "file"}
                        {sizeLabel && (
                            <>
                                <span aria-hidden> · </span>
                                <span data-testid="card-attachment-size">
                                    {sizeLabel}
                                </span>
                            </>
                        )}
                    </div>
                </div>
                {url && (
                    <a
                        className="btn btn-sm btn-primary execlaw-card-attachment__download"
                        href={url}
                        // `download` triggers save-as in browsers that
                        // honor the attribute; the server's
                        // Content-Disposition: attachment header is
                        // the authoritative trigger.
                        download={filename}
                        data-testid="card-attachment-download"
                    >
                        <i className="bi bi-download me-1" aria-hidden />
                        Download
                    </a>
                )}
            </div>
        </div>
    );
}

registerCardRenderer("attachment", AttachmentCard);
