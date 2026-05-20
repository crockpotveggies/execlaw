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

import { useContext, useEffect, useState } from "react";
import { AuthContext } from "../auth/AuthContext";
import { signDownloadUrl } from "../api/signedDownloadUrl";
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
    // 2026-05-19 — Browsers can't attach the Authorization header
    // to `<a download href>` link navigations. Pre-fix the SPA
    // pasted a raw JWT in `?access_token=<jwt>` — flagged by the
    // security audit (full-access JWTs leak via history,
    // referrers, copied links). Now we ask the server for a
    // signed URL bound to (path, user_id, exp) instead. Fetched
    // on mount; cached until the source path or auth changes.
    // A null `signedUrl` during the brief async window falls
    // through to `null` href so the Download button doesn't
    // render until the URL resolves.
    const getToken = auth?.getAccessToken;
    const [signedUrl, setSignedUrl] = useState<string | null>(null);
    useEffect(() => {
        if (!baseUrl || !getToken) {
            setSignedUrl(null);
            return;
        }
        let cancelled = false;
        signDownloadUrl(baseUrl, getToken)
            .then((u) => {
                if (!cancelled) setSignedUrl(u);
            })
            .catch(() => {
                // Sign failures (operator signed out, network blip,
                // etc.) leave the button absent rather than showing
                // a broken link. The card's next render after a
                // token refresh re-fetches.
                if (!cancelled) setSignedUrl(null);
            });
        return () => {
            cancelled = true;
        };
    }, [baseUrl, getToken]);
    const url = signedUrl;
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
