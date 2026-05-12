// Chat-component renderer registry.
//
// Symmetric to `cards/CardRenderer.tsx`'s registry pattern: per-kind
// renderers register at module-load time so MessageStream's dispatch
// path doesn't have to know which kinds the SPA defines.
//
// The dispatch contract: when a tool_result message's text payload
// parses as JSON and carries a `chat_component_kind` field, the
// MessageStream consults this registry. A registered component
// receives the parsed payload as `data`; unregistered kinds fall
// back to the default raw-JSON renderer.
//
// This is how plugin-shipped chat components (open-meteo's
// WeatherCurrent / WeatherDaily / Chart) land in the chat stream
// without each plugin needing to ship its own SPA bundle — they emit
// a payload shape; we register the renderer once in the SPA build.

import type { ComponentType } from "react";

/// Props every chat-component renderer accepts. `data` is the parsed
/// JSON payload the plugin emitted as its tool_result.
export interface ChatComponentProps {
    data: Record<string, unknown>;
}

const REGISTRY = new Map<string, ComponentType<ChatComponentProps>>();

export function registerChatComponent(
    kind: string,
    component: ComponentType<ChatComponentProps>,
): void {
    REGISTRY.set(kind, component);
}

export function getChatComponent(
    kind: string,
): ComponentType<ChatComponentProps> | undefined {
    return REGISTRY.get(kind);
}

/// Try to extract a chat_component_kind hint from a JSON-shaped tool
/// result. Returns `null` for non-JSON text or text that doesn't carry
/// the marker — MessageStream then falls back to the raw renderer.
export function detectChatComponent(text: string): {
    kind: string;
    data: Record<string, unknown>;
} | null {
    if (!text || !text.trim().startsWith("{")) return null;
    try {
        const parsed = JSON.parse(text);
        if (
            parsed &&
            typeof parsed === "object" &&
            !Array.isArray(parsed) &&
            typeof (parsed as Record<string, unknown>).chat_component_kind ===
                "string"
        ) {
            const kind = (parsed as Record<string, unknown>)
                .chat_component_kind as string;
            return { kind, data: parsed as Record<string, unknown> };
        }
    } catch {
        // Not JSON — fall through.
    }
    return null;
}
