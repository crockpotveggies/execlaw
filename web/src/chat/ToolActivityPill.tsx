// Inline loader that surfaces what the agent is doing right now,
// rendered as a transient row inside the message stream where the
// next assistant message would appear. Greyed-out text on the
// right of a small spinning execlaw mascot whose iris still
// tracks the cursor.
//
// Driven by the `agent_tool_activity` WS event (server-side
// `chats::humanise_tool_call`). The chat handler pushes a
// `started` pulse the instant the runner emits a tool-call
// request and a `finished`/`failed` pulse once the result is
// back. Replies + non-processing phase transitions both clear
// the entry.

import { MiniMascotSpinner } from "./MiniMascotSpinner";
import { useChatState, type ToolActivity } from "./store";

interface Props {
    conversationId: string;
}

export function ToolActivityPill({ conversationId }: Props) {
    const activity = useChatState((s) => s.toolActivity[conversationId]);
    if (!activity) return null;
    return (
        <div
            className="execlaw-msg execlaw-tool-activity"
            role="status"
            aria-live="polite"
            data-testid="tool-activity-pill"
            data-tool-name={activity.tool_name}
        >
            <div className="execlaw-tool-activity__row">
                <MiniMascotSpinner />
                <span
                    className="execlaw-tool-activity__label"
                    title={activity.label}
                >
                    {activity.label}
                </span>
            </div>
        </div>
    );
}

// Re-export for tests + Storybook-style isolation later.
export type { ToolActivity };
