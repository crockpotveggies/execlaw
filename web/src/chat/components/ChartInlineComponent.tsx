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

import { useContext } from "react";
import { AuthContext } from "../../auth/AuthContext";
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

    // Build the download URL for the PNG render. Browsers don't
    // attach the Authorization header to `<a download>` clicks, so
    // the SPA appends `?access_token=<jwt>` (same pattern as
    // AttachmentCard) when an auth context is available.
    const auth = useContext(AuthContext);
    const token = auth?.getAccessToken() ?? null;
    const downloadUrl =
        attachmentId !== null
            ? token
                ? `/api/attachments/${attachmentId}?access_token=${encodeURIComponent(
                      token,
                  )}`
                : `/api/attachments/${attachmentId}`
            : null;

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
