// Loader pill that shows the agent's current tool activity.
//
// Surfaces the per-conversation `agent_tool_activity` WS event
// (server-side `chats.rs::humanise_tool_call`). The chat handler
// pushes a `started` pulse into the store the instant the runner
// emits a tool-call request, and a `finished` (or `failed`) pulse
// once the result is back. Replies + non-processing phase
// transitions both clear the entry.
//
// Visually: small spinner on the left + the operator-facing label
// on the right, sitting just above the composer. Slides in/out via
// CSS rather than gsap to keep this component dependency-free.

import { useChatState, type ToolActivity } from "./store";

interface Props {
    conversationId: string;
}

export function ToolActivityPill({ conversationId }: Props) {
    const activity = useChatState((s) => s.toolActivity[conversationId]);
    if (!activity) return null;
    return (
        <div
            className="execlaw-tool-activity-pill"
            role="status"
            aria-live="polite"
            data-testid="tool-activity-pill"
            data-tool-name={activity.tool_name}
        >
            <span
                className="execlaw-tool-activity-pill__spinner"
                aria-hidden
            />
            <span
                className="execlaw-tool-activity-pill__label"
                title={activity.label}
            >
                {iconFor(activity.tool_name)}
                {activity.label}
            </span>
        </div>
    );
}

/// Optional emoji-as-icon next to the label. Kept inline rather
/// than pulled from bootstrap-icons because the pill is meant to
/// feel lightweight; switch to `<i className="bi bi-...">` later
/// if we want named icons. Falls through to no-icon for unknown
/// tools — the label is descriptive enough on its own.
function iconFor(toolName: string): string {
    if (toolName === "web_search") return "🔎 ";
    if (toolName === "web_fetch") return "🌐 ";
    if (toolName.startsWith("read_memory") || toolName === "list_memory")
        return "📓 ";
    if (toolName.startsWith("write_memory")) return "✍️ ";
    if (toolName.startsWith("read_chat_history") || toolName === "list_chats")
        return "💬 ";
    if (toolName === "notify_controller") return "🔔 ";
    if (toolName.startsWith("research_")) return "📚 ";
    if (toolName.startsWith("routine_")) return "⏰ ";
    if (toolName === "delegate_task") return "🤝 ";
    if (toolName.startsWith("calendar.")) return "📅 ";
    if (toolName.startsWith("contacts.")) return "👤 ";
    return "";
}

// Re-export for tests + Storybook-style isolation later.
export type { ToolActivity };
