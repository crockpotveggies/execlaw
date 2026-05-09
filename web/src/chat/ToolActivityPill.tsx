// Inline activity indicator that surfaces what the agent is doing
// right now, rendered as a transient row inside the message stream
// where the next assistant message will appear.
//
// The spinner and the label are decoupled:
//
//   * **Spinner** always renders while the agent is working on
//     this conversation (we just sent a message, OR the server's
//     phase is `thinking` / `awaiting_tool`). Hides only once
//     token-streaming starts (the streaming bubble below is
//     activity enough) or processing ends.
//
//   * **Label** is sticky. It appears next to the spinner when an
//     `agent_tool_activity` event arrives, and stays visible until
//     either a different tool's activity replaces it or token-
//     streaming starts. Pre-fix the label flashed on/off because
//     the WS handler cleared it on every `finished` pulse — felt
//     jittery; the operator sees the label briefly then nothing.
//     Now the label persists until something more useful takes
//     its place.

import { MiniMascotSpinner } from "./MiniMascotSpinner";
import { useChatState, type ToolActivity } from "./store";

interface Props {
    conversationId: string;
}

export function ToolActivityPill({ conversationId }: Props) {
    const isSending = useChatState(
        (s) => Boolean(s.sendingThreads[conversationId]),
    );
    const isProcessingThread = useChatState((s) => {
        const t = s.threads.find(
            (th) => th.conversation_id === conversationId,
        );
        return t?.is_processing ?? false;
    });
    const isStreaming = useChatState((s) => {
        const buf = s.streamingBuffer[conversationId];
        return typeof buf === "string" && buf.length > 0;
    });
    const activity = useChatState((s) => s.toolActivity[conversationId]);

    const working = isSending || isProcessingThread;
    // Once the streaming bubble takes over, that bubble IS the
    // "agent is talking now" affordance — hide the indicator so
    // we don't double up.
    if (!working || isStreaming) return null;

    return (
        <div
            className="execlaw-msg execlaw-tool-activity"
            role="status"
            aria-live="polite"
            data-testid="tool-activity-pill"
            data-tool-name={activity?.tool_name ?? ""}
        >
            <div className="execlaw-tool-activity__row">
                <MiniMascotSpinner />
                {activity ? (
                    <span
                        className="execlaw-tool-activity__label"
                        title={activity.label}
                    >
                        {activity.label}
                    </span>
                ) : null}
            </div>
        </div>
    );
}

// Re-export for tests + Storybook-style isolation later.
export type { ToolActivity };
