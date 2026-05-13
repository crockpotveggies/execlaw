// Brand-correct per-transport icons. Bootstrap Icons does ship
// brand glyphs for WhatsApp / Discord / Slack / Telegram (the
// official logos) but NOT for Signal Messenger — `bi-signal` is
// the generic cellular-meter glyph, not the messaging app. We
// inline the official Simple Icons SVG path for Signal
// (https://simpleicons.org/icons/signal — CC0).
//
// Resolution order (highest precedence first):
//   1. Known brand override — currently `signal` → SignalLogo.
//      Brand marks that bi-* can't represent live here. The
//      plugin manifest's `icon` field is IGNORED for channels in
//      this list because the brand SVG is canonical (operator
//      misconfiguring it shouldn't break the visual).
//   2. Plugin manifest's `icon` (passed in as `manifestIcon`).
//      A plugin author sets `icon = "discord"` and gets bi-discord;
//      a custom transport plugin can use any `bi-*` glyph.
//   3. Built-in channel → bi mapping for the standard channels
//      execlaw ships natively (web/email/voice/sms etc.). Keeps
//      the host honest when a manifest forgets the field.
//   4. Default fallback — `bi-chat-quote`. Operator-friendly
//      "this is a chat thread" generic.
//
// Sizing + alignment follow the existing `bi` icon convention
// (1em font-size, vertical-align: -0.125em) so a `<ChannelIcon>`
// drops into a flex row of bi-icons + text without offset jitter.

import type { CSSProperties } from "react";

/** Channels with a dedicated brand SVG that overrides the bi-* path. */
const BRAND_SVG_CHANNELS = new Set(["signal"]);

/** Built-in channel → bi-* mapping for transports that don't ship
 *  their own `icon = …` in the plugin manifest. Native channels
 *  (web/voice etc.) are the primary inhabitants; well-known
 *  brand-icon names also land here so a manifest typo doesn't
 *  produce a broken icon. Manifest values still take precedence
 *  via resolution-order step 2 above. */
const KNOWN_CHANNEL_BI: Record<string, string> = {
    web: "bi-globe",
    email: "bi-envelope",
    voice: "bi-mic",
    sms: "bi-phone",
    whatsapp: "bi-whatsapp",
    discord: "bi-discord",
    slack: "bi-slack",
    telegram: "bi-telegram",
    messenger: "bi-messenger",
};

const DEFAULT_FALLBACK_BI = "bi-chat-quote";

interface Props {
    /** Channel id — `"signal"`, `"whatsapp"`, `"discord"`, etc. The
     *  same string used as the principal-group's transport_channel
     *  and the plugin manifest's `[transport].transport_id`. */
    channel: string;
    /** Optional manifest-supplied icon name (sans `bi-` prefix).
     *  Used as the second-precedence source after BRAND_SVG_CHANNELS.
     *  Plugins set this via `[transport].icon` in plugin.toml; the
     *  host marshals it through `ThreadSummary.transport_icon`. */
    manifestIcon?: string | null;
    /** Override default size (1em). */
    size?: string | number;
    /** Suppress brand colour and use currentColor instead — useful
     *  when the icon needs to match surrounding muted-text. */
    monochrome?: boolean;
    /** Decorative-vs-meaningful: `aria-hidden` when this icon
     *  duplicates adjacent text, otherwise a label is provided. */
    decorative?: boolean;
    className?: string;
    "data-testid"?: string;
}

export function ChannelIcon({
    channel,
    manifestIcon = null,
    size = "1em",
    monochrome = false,
    decorative = false,
    className,
    "data-testid": testId,
}: Props) {
    const a11y = decorative
        ? { "aria-hidden": true as const }
        : { role: "img" as const, "aria-label": `channel: ${channel}` };
    const cls = ["execlaw-channel-icon", className].filter(Boolean).join(" ");
    const dataAttrs = {
        "data-testid": testId ?? "channel-icon",
        "data-channel": channel,
    };

    // Step 1: brand-SVG override.
    if (BRAND_SVG_CHANNELS.has(channel)) {
        return (
            <SignalLogo
                size={size}
                monochrome={monochrome}
                className={cls}
                a11y={a11y}
                dataAttrs={dataAttrs}
            />
        );
    }

    // Steps 2-4: bi-* resolution chain.
    const trimmed = (manifestIcon ?? "").trim();
    const biName =
        (trimmed.length > 0 ? `bi-${trimmed}` : undefined) ??
        KNOWN_CHANNEL_BI[channel] ??
        DEFAULT_FALLBACK_BI;

    const style: CSSProperties = { fontSize: size };
    return (
        <i
            className={`bi ${biName} ${cls}`}
            style={style}
            {...a11y}
            {...dataAttrs}
        />
    );
}

// Official Signal logomark — single-path SVG from simpleicons.org
// (CC0). The viewBox is the published 24x24 grid; we render the
// same path at any size via the height/width props. Brand color is
// `#3a76f0` (the "Signal Blue" used in the desktop client and on
// signal.org). When `monochrome` is set we drop the brand fill so
// the icon adopts the surrounding `currentColor` — useful in
// contexts that already encode "this is muted metadata."
function SignalLogo({
    size,
    monochrome,
    className,
    a11y,
    dataAttrs,
}: {
    size: string | number;
    monochrome: boolean;
    className?: string;
    a11y: Record<string, unknown>;
    dataAttrs: Record<string, string>;
}) {
    const fill = monochrome ? "currentColor" : "#3a76f0";
    return (
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            width={size}
            height={size}
            className={className}
            style={{ verticalAlign: "-0.125em" }}
            {...a11y}
            {...dataAttrs}
        >
            <path
                d="M9.12.35a11.84 11.84 0 0 0-2.6.969l.9 1.8A9.715 9.715 0 0 1 9.6 2.34zm5.76 0-.477 1.99a9.7 9.7 0 0 1 2.18.778l.901-1.8a11.7 11.7 0 0 0-2.604-.968zM4.794 2.16A12.05 12.05 0 0 0 2.91 4.046l1.582 1.276c.456-.564.97-1.077 1.533-1.534zm14.413 0L17.93 3.788c.563.456 1.077.97 1.533 1.532l1.583-1.276a12.05 12.05 0 0 0-1.838-1.882zM1.319 6.522A11.85 11.85 0 0 0 .35 9.121l1.99.477c.182-.756.443-1.485.778-2.18zm21.36 0-1.797.898c.337.696.598 1.426.78 2.181l1.988-.476a11.7 11.7 0 0 0-.97-2.604zM12 2.4A9.6 9.6 0 0 0 3.685 16.812L2.428 21.57l4.758-1.257a9.6 9.6 0 1 0 4.804-17.91l.011-.001zM.062 10.681a11.7 11.7 0 0 0 0 2.78l1.99-.255a9.6 9.6 0 0 1 0-2.27zm21.886 0-1.989.255a9.6 9.6 0 0 1 0 2.27l1.99.254c.117-.92.117-1.85 0-2.776v-.003zM2.34 14.4l-1.99.475c.224.918.55 1.79.969 2.604l1.797-.898A9.6 9.6 0 0 1 2.34 14.4zm19.32 0c-.182.755-.443 1.485-.778 2.18l1.798.901a12 12 0 0 0 .969-2.604zM4.494 17.93l-1.583 1.277a12 12 0 0 0 1.881 1.881l1.276-1.582a9.6 9.6 0 0 1-1.534-1.535v-.04zm15.012 0a9.6 9.6 0 0 1-1.531 1.534l1.275 1.583a12 12 0 0 0 1.881-1.881zm-12.69 2.343-.901 1.798c.815.42 1.687.745 2.604.969l.477-1.989a9.7 9.7 0 0 1-2.18-.778zm10.368 0c-.696.336-1.426.597-2.181.779l.477 1.99c.918-.225 1.789-.55 2.604-.97zm-7.66.95-.255 1.989a11.7 11.7 0 0 0 2.78 0l-.255-1.99a9.6 9.6 0 0 1-2.27 0z"
                fill={fill}
            />
        </svg>
    );
}
