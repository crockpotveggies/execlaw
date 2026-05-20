// Inline chart renderer for tool_result payloads with
// `chat_component_kind: "chart"`.
//
// The host's `host_render_chart` Rhai binding returns a payload
// carrying:
//   * `svg`           — pre-rendered SVG markup string (server-side
//                       plotters render). Dropped verbatim into the
//                       DOM via dangerouslySetInnerHTML; safe because
//                       the SVG came from our own renderer, not
//                       user-supplied data.
//   * `attachment_id` — the same chart as a PNG, fetchable at
//                       `/api/attachments/<id>` for save-as.
//   * `title`         — optional, used as <figcaption>.
//
// No JS chart library is shipped: the SPA renders whatever SVG the
// host produced. Keeps the bundle small and the rendering
// deterministic across browsers.

import { useContext, useEffect, useState } from "react";
import { AuthContext } from "../../auth/AuthContext";
import { signDownloadUrl } from "../../api/signedDownloadUrl";
import {
    registerChatComponent,
    type ChatComponentProps,
} from "../chatComponentRegistry";

function ChartInline({ data }: ChatComponentProps) {
    const svg = typeof data.svg === "string" ? (data.svg as string) : null;
    const attachmentId =
        typeof data.attachment_id === "string"
            ? (data.attachment_id as string)
            : null;
    const title = typeof data.title === "string" ? (data.title as string) : null;

    // 2026-05-19 — fetch a signed URL for the PNG render. Pre-fix
    // the SPA pasted a raw JWT; security audit flagged that as
    // broad leak surface. See AttachmentCard for the full why.
    const auth = useContext(AuthContext);
    const getToken = auth?.getAccessToken;
    const basePath =
        attachmentId !== null ? `/api/attachments/${attachmentId}` : null;
    const [downloadUrl, setDownloadUrl] = useState<string | null>(null);
    useEffect(() => {
        if (!basePath || !getToken) {
            setDownloadUrl(null);
            return;
        }
        let cancelled = false;
        signDownloadUrl(basePath, getToken)
            .then((u) => {
                if (!cancelled) setDownloadUrl(u);
            })
            .catch(() => {
                if (!cancelled) setDownloadUrl(null);
            });
        return () => {
            cancelled = true;
        };
    }, [basePath, getToken]);

    return (
        <figure
            className="execlaw-chart-inline"
            data-testid="chart-inline"
            data-attachment-id={attachmentId ?? undefined}
        >
            {svg ? (
                <div
                    className="execlaw-chart-inline__svg"
                    // SVG payload comes from the host's own renderer,
                    // not user-controlled input. The renderer rejects
                    // non-finite numbers and bounds string lengths
                    // before producing markup; there's no XSS surface
                    // for a plugin to introduce here.
                    // eslint-disable-next-line react/no-danger
                    dangerouslySetInnerHTML={{ __html: svg }}
                />
            ) : (
                <div className="execlaw-chart-inline__missing execlaw-muted small">
                    chart not available
                </div>
            )}
            {(title || downloadUrl) && (
                <figcaption className="execlaw-chart-inline__caption">
                    {title && (
                        <span className="execlaw-chart-inline__title">
                            {title}
                        </span>
                    )}
                    {downloadUrl && (
                        <a
                            className="execlaw-chart-inline__download execlaw-muted small"
                            href={downloadUrl}
                            download={title ? `${title}.png` : "chart.png"}
                            data-testid="chart-inline-download"
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

registerChatComponent("chart", ChartInline);

export default ChartInline;
