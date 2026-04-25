import { describe, expect, it, vi } from "vitest";
import { WsClient } from "../api/ws";

describe("WsClient", () => {
    it("dispatches valid JSON events to the listener", () => {
        const seen: unknown[] = [];
        const client = new WsClient({
            accessToken: () => null,
            onEvent: (ev) => seen.push(ev),
            urlOverride: "ws://localhost:1/ignore",
        });
        client.handleRawMessage(
            JSON.stringify({
                kind: "ChatTokenDelta",
                conversation_id: "c1",
                delta: "hi",
            }),
        );
        expect(seen).toHaveLength(1);
        expect((seen[0] as { kind: string }).kind).toBe("ChatTokenDelta");
        client.close();
    });

    it("ignores non-string payloads silently", () => {
        const onEvent = vi.fn();
        const client = new WsClient({
            accessToken: () => null,
            onEvent,
            urlOverride: "ws://localhost:1/ignore",
        });
        client.handleRawMessage(new Uint8Array([1, 2, 3]) as unknown as string);
        client.handleRawMessage(42 as unknown as string);
        expect(onEvent).not.toHaveBeenCalled();
        client.close();
    });

    it("ignores malformed JSON without throwing", () => {
        const onEvent = vi.fn();
        const client = new WsClient({
            accessToken: () => null,
            onEvent,
            urlOverride: "ws://localhost:1/ignore",
        });
        expect(() => client.handleRawMessage("{not json")).not.toThrow();
        expect(onEvent).not.toHaveBeenCalled();
        client.close();
    });

    it("ignores parsed payloads without a `kind` string", () => {
        const onEvent = vi.fn();
        const client = new WsClient({
            accessToken: () => null,
            onEvent,
            urlOverride: "ws://localhost:1/ignore",
        });
        client.handleRawMessage(JSON.stringify({}));
        client.handleRawMessage(JSON.stringify({ kind: 42 }));
        expect(onEvent).not.toHaveBeenCalled();
        client.close();
    });
});
