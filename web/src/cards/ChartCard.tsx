// Per-kind renderer for `kind: chart` cards.
//
// 2026-05-16 — chart rendering used to flow through the inline chat-
// component dispatch (`chat_component_kind: "chart"` in tool_result
// text → ChartInlineComponent). That path proved unreliable —
// the chart never visually appeared in chat even when the tool
// fired and the JSON was correct. Promoted to a first-class card
// so it rides the same proven pipeline that deep-research
// progress and send_attachment chips use:
//
//   tool emits CardOpened (kind=Chart, details={svg, attachment_id, ...})
//   → server cards-projection persists + broadcasts on the WS bus
//   → SPA's CardStore picks it up
//   → MessageStream interleaves it with messages by event_seq
//   → this renderer dispatched by getCardRenderer("chart")
//
// Symmetric to AttachmentCard's chip pattern; the difference is
// that the chart card embeds the SVG inline (the renderer's own
// output, not user data, so dangerouslySetInnerHTML is safe) and
// adds a "PNG" download chip pointing at the persisted artifact.

import { useContext, useEffect, useState } from "react";
import { AuthContext } from "../auth/AuthContext";
import { signDownloadUrl } from "../api/signedDownloadUrl";
import { registerCardRenderer, type CardRendererProps } from "./CardRenderer";

interface ChartDetails {
    attachment_id?: string;
    filename?: string;
    title?: string | null;
    svg?: string;
    width?: number;
    height?: number;
    download_url?: string;
    mime_type?: string;
}

function readDetails(raw: unknown): ChartDetails | null {
    if (!raw || typeof raw !== "object") return null;
    return raw as ChartDetails;
}

export function ChartCard({ card }: CardRendererProps) {
    const auth = useContext(AuthContext);
    const details = readDetails(card.details);
    const svg = typeof details?.svg === "string" ? details.svg : null;
    const attachmentId =
        typeof details?.attachment_id === "string"
            ? details.attachment_id
            : null;
    const title = details?.title || card.title || null;
    const filename = details?.filename ?? "chart.png";

    // 2026-05-19 — ask the server for a signed URL instead of
    // pasting a raw JWT in the query string. See AttachmentCard
    // for the why.
    const baseUrl =
        details?.download_url ??
        (attachmentId ? `/api/attachments/${attachmentId}` : null);
    const getToken = auth?.getAccessToken;
    const [downloadUrl, setDownloadUrl] = useState<string | null>(null);
    useEffect(() => {
        if (!baseUrl || !getToken) {
            setDownloadUrl(null);
            return;
        }
        let cancelled = false;
        signDownloadUrl(baseUrl, getToken)
            .then((u) => {
                if (!cancelled) setDownloadUrl(u);
            })
            .catch(() => {
                if (!cancelled) setDownloadUrl(null);
            });
        return () => {
            cancelled = true;
        };
    }, [baseUrl, getToken]);

    return (
        <figure
            className="execlaw-card-chart"
            data-testid="card-chart"
            data-card-id={card.card_id}
            data-attachment-id={attachmentId ?? undefined}
        >
            {svg ? (
                <div
                    className="execlaw-card-chart__svg"
                    // SVG comes from the host's plotters renderer, NOT
                    // user-controlled input. The renderer rejects
                    // non-finite numbers and bounds string lengths
                    // before producing markup; there's no XSS surface
                    // for a plugin to introduce here.
                    // eslint-disable-next-line react/no-danger
                    dangerouslySetInnerHTML={{ __html: svg }}
                />
            ) : (
                <div className="execlaw-card-chart__missing execlaw-muted small">
                    chart not available
                </div>
            )}
            {(title || downloadUrl) && (
                <figcaption className="execlaw-card-chart__caption">
                    {title && (
                        <span
                            className="execlaw-card-chart__title"
                            data-testid="card-chart-title"
                        >
                            {title}
                        </span>
                    )}
                    {downloadUrl && (
                        <a
                            className="execlaw-card-chart__download execlaw-muted small"
                            href={downloadUrl}
                            download={filename}
                            data-testid="card-chart-download"
                        >
                            <i className="bi bi-download me-1" aria-hidden />
                            PNG
                        </a>
                    )}
                </figcaption>
            )}
        </figure>
    );
}

registerCardRenderer("chart", ChartCard);

export default ChartCard;
