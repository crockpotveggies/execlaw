// Vertically scrolling message list for the active thread.
//
// Auto-scrolls to the latest message on append IF the operator is
// already at the bottom. When the operator has scrolled up to read
// history, incoming messages no longer yank the viewport — instead
// a small ↓ button surfaces above the composer; clicking it snaps
// to the latest. Long messages render in full — no clamp, no
// "Read more…" affordance (the Phase-6 truncation spec was reverted
// 2026-05-15: in practice, a click-to-expand hides important
// context behind a friction the operator never asked for, and the
// drafted long-form replies the agent produces are exactly the
// content the operator wants to read inline). Each bubble also
// renders a subtle channel-origin icon (signal / email / voice) so
// the controller can see at a glance which transport delivered the
// message.

import {
    useCallback,
    useContext,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import type { MessageView } from "../api/endpoints";
import { AuthContext } from "../auth/AuthContext";
import { getCardRenderer } from "../cards/CardRenderer";
import { useCardsForConversation } from "../cards/cardStore";
import type { Card } from "../cards/types";
import { ChannelIcon } from "../components/ChannelIcons";
import { MarkdownContent } from "../components/MarkdownContent";
import { ToolActivityPill } from "./ToolActivityPill";
import {
    detectChatComponent,
    getChatComponent,
} from "./chatComponentRegistry";
// Side-effect imports — each module self-registers its renderer with
// the chat-component registry at module-load. Listed here so the
// bundler keeps them in the SPA build; without this they'd be tree-
// shaken since no symbol from them is referenced directly.
import "./components/ChartInlineComponent";
import "./components/WeatherCurrentComponent";
import "./components/WeatherDailyComponent";
import "./components/PythonExecuteComponent";
import { useChatState } from "./store";

// 2026-05-18 — helpers for the file-vs-image branching when
// rendering attachments under user message bubbles. Image MIMEs
// mirror the server's `ALLOWED_ATTACHMENT_MIMES` image subset;
// anything else takes the file-chip path.
const IMAGE_MIME_SET: ReadonlySet<string> = new Set([
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
]);
function isImageMime(m: string): boolean {
    return IMAGE_MIME_SET.has(m);
}

/// Bootstrap-icons class for a non-image attachment chip. Mirrors
/// the Composer's `fileIconForMime` so a CSV looks the same in the
/// composer preview and the rendered message.
function fileIconForMime(mime: string): string {
    if (mime === "application/pdf") return "bi bi-file-earmark-pdf";
    if (mime === "text/csv" || mime === "text/tab-separated-values")
        return "bi bi-file-earmark-spreadsheet";
    if (
        mime ===
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" ||
        mime === "application/vnd.ms-excel"
    )
        return "bi bi-file-earmark-excel";
    if (mime === "application/json") return "bi bi-file-earmark-code";
    if (mime === "text/markdown") return "bi bi-markdown";
    if (mime === "text/plain") return "bi bi-file-earmark-text";
    return "bi bi-file-earmark";
}

/// Compact byte-size formatter for the message-attachment chip.
/// Matches the Composer's `formatBytes` style ("5.2 KB", "1.4 MB").
function formatBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

interface Props {
    conversationId: string;
    /**
     * 2026-05-16 — when false, `tool_use` / `tool_result` messages
     * are filtered out of the stream entirely (not just hidden via
     * CSS — dropped from the items list so the DOM stays small).
     * Default `true` preserves historical behaviour for tests + any
     * caller that doesn't thread the toggle. The operator flips
     * this via the chat header's view-filter popup; the preference
     * persists across reloads (see `useToolResultsVisible`).
     */
    showToolResults?: boolean;
}

/** Pixel slack on the at-bottom check. Browsers can leave a fractional
 *  gap between scrollTop+clientHeight and scrollHeight even when the
 *  user is visually at the floor; 8 px swallows that noise. */
const AT_BOTTOM_SLACK_PX = 8;

/// Discriminated union the chronological-render loop walks. Cards
/// and messages are interleaved by their wall-clock timestamp.
type StreamItem =
    | { kind: "message"; message: MessageView; sortKey: number }
    | { kind: "card"; card: Card; sortKey: number };

export function MessageStream({ conversationId, showToolResults = true }: Props) {
    const messages = useChatState(
        (s) => s.messages[conversationId] ?? null,
    );
    const streaming = useChatState(
        (s) => s.streamingBuffer[conversationId] ?? null,
    );
    const cards = useCardsForConversation(conversationId);
    const items: StreamItem[] = useMemo(() => {
        const acc: StreamItem[] = [];
        if (messages) {
            for (const m of messages) {
                // 2026-05-16 — tool_use / tool_result bubbles are
                // typically raw JSON / monospace dumps that crowd
                // the transcript when the operator just wants the
                // agent's prose. The chat header's view filter
                // gates them on/off; default is on.
                if (!showToolResults && isToolKind(m.kind)) continue;
                acc.push({ kind: "message", message: m, sortKey: m.seq });
            }
        }
        // 2026-05-03 — cards now arrive with their real
        // `state_events.seq` from the WS payload (`event_seq`).
        // Use it as the sortKey so cards land inline at their
        // true position in the chat-thread timeline, alongside
        // messages, instead of being pinned to the tail.
        //
        // Fallback for legacy WS payloads / fixtures with no seq:
        // synthesise a key from `opened_at` × 1e6 so the card
        // still lands chronologically (and at the bottom for
        // `event_seq`-less cards opened in the past, matching
        // the prior behavior).
        for (const c of cards) {
            const sortKey =
                c.event_seq !== null && c.event_seq !== undefined
                    ? c.event_seq
                    : c.opened_at * 1_000_000;
            acc.push({
                kind: "card",
                card: c,
                sortKey,
            });
        }
        acc.sort((a, b) => a.sortKey - b.sortKey);
        return acc;
    }, [messages, cards, showToolResults]);
    const scrollRef = useRef<HTMLDivElement | null>(null);
    const [isAtBottom, setIsAtBottom] = useState(true);

    // Auto-stick to the bottom only when the operator is already
    // there. Mid-history scroll-up means "I'm reading older content,
    // don't yank me away" — incoming messages just pile under the
    // viewport and the ↓ button surfaces.
    useEffect(() => {
        const el = scrollRef.current;
        if (!el) return;
        if (isAtBottom) {
            el.scrollTop = el.scrollHeight;
        }
    }, [messages, streaming, isAtBottom]);

    const onScroll = useCallback(() => {
        const el = scrollRef.current;
        if (!el) return;
        const distanceFromBottom =
            el.scrollHeight - el.scrollTop - el.clientHeight;
        setIsAtBottom(distanceFromBottom <= AT_BOTTOM_SLACK_PX);
    }, []);

    const scrollToBottom = useCallback(() => {
        const el = scrollRef.current;
        if (!el) return;
        el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    }, []);

    // Re-establish at-bottom on conversation switch so the new
    // thread starts in autoscroll mode.
    useEffect(() => {
        setIsAtBottom(true);
    }, [conversationId]);

    if (messages === null) {
        return (
            <div className="execlaw-stream-wrap">
                <div className="execlaw-stream" ref={scrollRef}>
                    <div className="execlaw-empty-state small">
                        Loading messages…
                    </div>
                </div>
            </div>
        );
    }

    if (messages.length === 0 && cards.length === 0 && !streaming) {
        return (
            <div className="execlaw-stream-wrap">
                <div className="execlaw-stream" ref={scrollRef}>
                    <div className="execlaw-empty-state">
                        <i
                            className="bi bi-chat-square-dots"
                            style={{
                                fontSize: "2rem",
                                display: "block",
                                marginBottom: "0.5rem",
                            }}
                            aria-hidden
                        />
                        No messages yet. Type below to start.
                    </div>
                </div>
            </div>
        );
    }

    return (
        <div className="execlaw-stream-wrap">
            <div
                className="execlaw-stream"
                ref={scrollRef}
                onScroll={onScroll}
                data-testid="message-stream"
            >
                {items.map((item) => {
                    if (item.kind === "message") {
                        const m = item.message;
                        return (
                            <MessageBubble
                                key={`msg-${m.kind}-${m.seq}`}
                                message={m}
                            />
                        );
                    }
                    const Renderer = getCardRenderer(item.card.kind);
                    // 2026-05-04 — wrap every card in `.execlaw-msg`
                    // so it inherits the same centered + clamped
                    // reading-column treatment that messages get via
                    // MessageBubble. Without this wrapper, cards
                    // (research card, attachment chip, etc.) anchored
                    // to the outer scroll surface and ran the full
                    // viewport width — out of alignment with the
                    // surrounding chat bubbles. The renderer's own
                    // outer `<div className="execlaw-card-…">` keeps
                    // its kind-specific styling; the wrapper just
                    // owns column placement.
                    return (
                        <div
                            key={`card-${item.card.card_id}`}
                            className="execlaw-msg execlaw-msg--card"
                        >
                            <Renderer card={item.card} />
                        </div>
                    );
                })}
                {/* Live "what's the agent doing" pulse, mounted
                    inside the message stream where the next
                    assistant message will land. Shows nothing
                    when there's no active tool — only renders
                    while a `tool_call` round-trip is in flight. */}
                <ToolActivityPill conversationId={conversationId} />
                {streaming && (
                    <div className="execlaw-msg" data-testid="streaming-bubble">
                        <div className="execlaw-msg__meta">
                            agent · streaming
                            <span className="execlaw-streaming-cursor" aria-hidden>
                                ▍
                            </span>
                        </div>
                        <div className="execlaw-msg__bubble">
                            <MarkdownContent text={streaming} streaming />
                        </div>
                    </div>
                )}
            </div>
            {!isAtBottom && (
                <button
                    type="button"
                    className="execlaw-scroll-to-bottom"
                    onClick={scrollToBottom}
                    aria-label="Scroll to latest message"
                    data-testid="scroll-to-bottom"
                >
                    <i className="bi bi-arrow-down" aria-hidden />
                </button>
            )}
        </div>
    );
}

function MessageBubble({ message }: { message: MessageView }) {
    // 2026-05-15 — read AuthContext directly (not via the `useAuth()`
    // wrapper that throws when there's no provider). MessageStream
    // unit tests render the component in isolation without an
    // `<AuthProvider>`; gracefully degrading to `null` keeps those
    // tests passing — the only consequence is that the access-token
    // query param on `<img>` attachment URLs is absent, which would
    // 401 against the live server but doesn't affect those tests.
    const auth = useContext(AuthContext);
    const role = roleFor(message);
    // 2026-05-15 — when the operator picked a skill from the
    // composer's `+` menu, the server prepended a `<skill
    // name="...">...</skill>\n\n` block onto the user_msg text
    // before the model saw it. Strip those blocks here so the chat
    // bubble shows only what the operator actually typed; the
    // skills surface as chips below the bubble instead. Stripping
    // is keyed off `applied_skill_names` being non-empty so we
    // never accidentally trim text the user actually typed that
    // happens to start with `<skill ...>`.
    const appliedSkills = message.applied_skill_names ?? [];
    const rawText = message.text ?? renderToolFallback(message);
    const text =
        appliedSkills.length > 0
            ? stripSkillPrependBlock(rawText)
            : rawText;
    const channelOrigin = readChannelOrigin(message);

    // Long messages render in full — see the file-header note for
    // why we dropped the Phase-6 clamp + "Read more" affordance.

    // Rich tool-result rendering: if a tool_result message's payload
    // is JSON carrying a `chat_component_kind` marker and a renderer
    // is registered for that kind, render via the chat-component
    // registry instead of dumping raw JSON. Falls back to plain text
    // for unknown kinds or non-JSON payloads.
    const chatComponent =
        message.kind === "tool_result" ? detectChatComponent(text) : null;
    const ChatComponentRenderer = chatComponent
        ? getChatComponent(chatComponent.kind)
        : undefined;

    // 2026-04-28 — meta-line cleanup:
    //   * Hide the channel-origin icon for the default `web` origin
    //     (adds visual noise; only useful when a non-web transport
    //     delivered the message — Signal / email / voice / sms).
    //   * Skip the `· {actor}` suffix when the actor is redundant
    //     with the role — "agent · agent" was the regression we
    //     caught here. The runner stamps `actor: "agent"` on every
    //     `model_turn` event; collapsing that into just "agent" reads
    //     cleaner.
    //   * Drop the meta entirely for the common case (web-origin
    //     user message). The right-aligned pill already telegraphs
    //     "you said this" — a "you" label adds no info. We DO keep
    //     the meta when a user message arrives via a non-web
    //     transport (Signal / email / voice / sms) so the operator
    //     can see "this came in over Signal" without inspecting the
    //     event payload.
    const showOriginIcon = channelOrigin !== "web";
    const actorSuffix =
        message.actor && message.actor !== role
            ? ` · ${message.actor}`
            : "";
    const isUserMessage = message.kind === "user_msg";
    const showMeta = !isUserMessage || channelOrigin !== "web";

    // 2026-05-15 — image attachments. The server emits these on
    // user_msg events the operator submitted through the composer's
    // `+` menu. Each entry has an `id` and `mime`; the id is either:
    //   * a persisted attachment id (server-minted) → served via
    //     `/api/attachments/<id>` and gated by the auth cookie /
    //     bearer the SPA already holds, OR
    //   * a `data:` URL when the entry is still optimistic (the SPA
    //     just sent it; canonical id arrives once listMessages
    //     refetches). The `<img>` accepts both verbatim.
    const attachments = message.attachments ?? [];

    return (
        <div
            className={"execlaw-msg" + (isUserMessage ? " is-user" : "")}
        >
            {showMeta && (
                <div className="execlaw-msg__meta">
                    {showOriginIcon && (
                        <ChannelOriginIcon origin={channelOrigin} />
                    )}
                    {role}
                    {actorSuffix}
                </div>
            )}
            <div
                className={
                    "execlaw-msg__bubble" +
                    (isUserMessage ? " is-user" : "") +
                    (isToolKind(message.kind) ? " is-tool" : "") +
                    (ChatComponentRenderer ? " is-rich-component" : "")
                }
            >
                {attachments.length > 0 && (
                    <div
                        className="execlaw-msg__attachments"
                        data-testid="message-attachments"
                    >
                        {attachments.map((a) => {
                            // 2026-05-15 — `<img>` tags can't attach
                            // the SPA's Authorization: Bearer header,
                            // so a plain `/api/attachments/{id}` src
                            // gets blocked by `AuthedUser` and the
                            // browser falls back to the alt text.
                            // The auth extractor supports a
                            // `?access_token=<jwt>` query fallback
                            // specifically for browser-direct GETs;
                            // we use it here for persisted ids.
                            // Optimistic data URLs (in-flight upload)
                            // are still rendered verbatim.
                            const src = a.id.startsWith("data:")
                                ? a.id
                                : buildAttachmentSrc(
                                      a.id,
                                      auth?.getAccessToken() ?? null,
                                  );
                            // 2026-05-18 — branch on MIME. Non-image
                            // attachments (CSV / PDF / JSON / etc.)
                            // can't render as `<img>` — the browser
                            // shows the alt text "attached image" and
                            // the operator sees a broken visual. File
                            // chip with icon + filename + download
                            // link is the correct affordance for the
                            // non-vision path.
                            if (isImageMime(a.mime)) {
                                return (
                                    <img
                                        key={a.id}
                                        src={src}
                                        alt="attached image"
                                        className="execlaw-msg__attachment-image"
                                    />
                                );
                            }
                            return (
                                <a
                                    key={a.id}
                                    href={src}
                                    download={a.filename ?? undefined}
                                    className="execlaw-msg__attachment-file"
                                    data-testid="message-attachment-file"
                                    data-mime={a.mime}
                                    title={`${a.filename ?? "file"} · ${a.mime}`}
                                >
                                    <i
                                        className={fileIconForMime(a.mime)}
                                        aria-hidden
                                    />
                                    <span className="execlaw-msg__attachment-file-name">
                                        {a.filename ?? "attachment"}
                                    </span>
                                    {typeof a.size_bytes === "number" &&
                                        a.size_bytes > 0 && (
                                            <span className="execlaw-msg__attachment-file-size">
                                                {formatBytes(a.size_bytes)}
                                            </span>
                                        )}
                                </a>
                            );
                        })}
                    </div>
                )}
                {/* Tool messages are JSON / monospace dumps by default —
                    render as raw text. Exception: when the payload
                    carries a `chat_component_kind` marker AND a
                    matching renderer is registered (open-meteo's
                    weather / chart components), dispatch to the
                    component instead. Everything else (agent + user)
                    goes through the markdown pipeline. */}
                {text.length > 0 &&
                    (ChatComponentRenderer && chatComponent ? (
                        <ChatComponentRenderer data={chatComponent.data} />
                    ) : isToolKind(message.kind) ? (
                        text
                    ) : (
                        <MarkdownContent text={text} />
                    ))}
                {appliedSkills.length > 0 && (
                    <div
                        className="execlaw-msg__applied-skills"
                        data-testid="message-applied-skills"
                    >
                        <i className="bi bi-stars" aria-hidden />
                        <span className="execlaw-msg__applied-skills-label">
                            applied:
                        </span>
                        {appliedSkills.map((name, i) => (
                            <span
                                key={name}
                                className="execlaw-msg__applied-skill-chip"
                                data-testid="message-applied-skill"
                                data-skill-name={name}
                            >
                                {name}
                                {i < appliedSkills.length - 1 ? "," : ""}
                            </span>
                        ))}
                    </div>
                )}
            </div>
        </div>
    );
}

/// Strip the leading `<skill name="...">...</skill>` blocks the
/// server prepends onto a user_msg when the operator picked skills
/// from the composer's `+` menu. The block format is fixed:
///
/// ```text
/// <skill name="ns/short">
/// {body}
/// </skill>
///
/// {original user text}
/// ```
///
/// We match one OR MORE leading blocks (multi-skill picks) followed
/// by any blank lines, then return whatever's left. Caller only
/// invokes this when `applied_skill_names` is non-empty so we never
/// trim text the user typed that happens to look like a skill
/// block.
export function stripSkillPrependBlock(text: string): string {
    // Greedy match for any number of consecutive `<skill name="X">
    // ... </skill>` blocks at the very start of the text, plus any
    // trailing whitespace before the user's actual content. The
    // `[\s\S]*?` (non-greedy) inside each block stops at the first
    // `</skill>` so we don't accidentally swallow user text that
    // contains the literal substring.
    const stripped = text.replace(
        /^(<skill name="[^"]*">[\s\S]*?<\/skill>\s*)+/,
        "",
    );
    return stripped;
}

function readChannelOrigin(m: MessageView): ChannelOrigin {
    // The server attaches a channel_origin field to user_msg +
    // model_turn events that flowed through a transport bridge
    // (signal / email / voice / sms). Web-originated turns leave it
    // absent; the SPA defaults to "web" and shows no icon.
    const raw = (m as MessageView & { channel_origin?: unknown }).channel_origin;
    if (raw === "signal" || raw === "email" || raw === "voice" || raw === "sms") {
        return raw;
    }
    return "web";
}

type ChannelOrigin = "web" | "signal" | "email" | "voice" | "sms";

function ChannelOriginIcon({ origin }: { origin: ChannelOrigin }) {
    return (
        <ChannelIcon
            channel={origin}
            size="1em"
            decorative={false}
            className="execlaw-channel-origin me-2"
            data-testid="channel-origin"
        />
    );
}

function roleFor(m: MessageView): string {
    switch (m.kind) {
        case "user_msg":
            return "you";
        case "model_turn":
            return "agent";
        case "tool_use":
            return "tool · request";
        case "tool_result":
            return "tool · result";
        default:
            return m.kind;
    }
}

function isToolKind(kind: string): boolean {
    return kind === "tool_use" || kind === "tool_result";
}

function renderToolFallback(m: MessageView): string {
    return `[${m.kind} (no text payload)]`;
}

/// Build an `/api/attachments/<id>` URL with the access token in the
/// query string. The auth extractor (`AuthedUser`) accepts
/// `?access_token=<jwt>` as a fallback for browser-direct GETs like
/// `<img>` tags, which can't carry an Authorization: Bearer header.
/// A null token (e.g. just-rotated, render before refresh lands)
/// falls back to the bare URL — the request will 401, the `<img>`
/// shows alt text briefly, and the next render after token rotation
/// re-renders with the new token.
function buildAttachmentSrc(id: string, token: string | null): string {
    const path = `/api/attachments/${encodeURIComponent(id)}`;
    return token ? `${path}?access_token=${encodeURIComponent(token)}` : path;
}
