// Pure-state-update helpers exposed from AutomationCanvas. The canvas
// itself can't be exercised in jsdom (ReactFlow needs real DOM
// measurement), but every mutation is funneled through these
// functions so testing them is testing the canvas's behaviour.
//
// These helpers are the only place the canvas mutates an
// AutomationDef — once they're correct, drag/drop/connect/rename in
// the running UI become thin event handlers.

import { describe, it, expect, vi } from "vitest";

// AutomationCanvas.tsx imports `@xyflow/react` at module load — mock
// it so the helpers can be loaded under jsdom without dragging in the
// real library's measurement code paths.
vi.mock("@xyflow/react", () => ({
    ReactFlow: () => null,
    ReactFlowProvider: ({ children }: { children?: unknown }) => children,
    Background: () => null,
    Controls: () => null,
    Handle: () => null,
    Position: { Top: "top", Bottom: "bottom", Left: "left", Right: "right" },
    addEdge: (_c: unknown, edges: unknown[]) => edges,
    // The helpers test doesn't render the canvas, so these hooks
    // never get called — they just need to be importable.
    useNodesState: () => [[], () => {}, () => {}],
    useEdgesState: () => [[], () => {}, () => {}],
    useReactFlow: () => ({
        screenToFlowPosition: ({ x, y }: { x: number; y: number }) => ({ x, y }),
    }),
}));

import {
    withAddedEdge,
    withRemovedEdge,
    withRemovedNode,
    withRenamedNode,
    withUpdatedNode,
    withUpdatedPosition,
} from "../settings/AutomationCanvas";
import type { AutomationDef, NodeDef } from "../api/automations";

function fixtureDef(): AutomationDef {
    return {
        trigger: { kind: "webhook.received", when: null },
        nodes: [
            { id: "f1", kind: "Filter", config: { expr: "true" } },
            { id: "t1", kind: "Transform", config: { expr: "#{}" } },
            { id: "end", kind: "Terminal", config: {} },
        ],
        edges: [
            { from: "trigger", to: "f1", when: null },
            { from: "f1", to: "t1", when: null },
            { from: "t1", to: "end", when: null },
        ],
    };
}

describe("withUpdatedPosition", () => {
    it("sets position on the matching node, leaves others untouched", () => {
        const def = fixtureDef();
        const next = withUpdatedPosition(def, "t1", { x: 100, y: 200 });
        expect(next.nodes.find((n) => n.id === "t1")?.position).toEqual({
            x: 100,
            y: 200,
        });
        // Other nodes' positions are unchanged (still undefined).
        expect(next.nodes.find((n) => n.id === "f1")?.position).toBeUndefined();
    });

    it("is a no-op for unknown ids (returns equivalent def)", () => {
        const def = fixtureDef();
        const next = withUpdatedPosition(def, "ghost", { x: 5, y: 5 });
        expect(next.nodes).toEqual(def.nodes);
    });
});

describe("withRemovedNode", () => {
    it("removes the node and prunes inbound + outbound edges", () => {
        const def = fixtureDef();
        const next = withRemovedNode(def, "t1");
        expect(next.nodes.map((n) => n.id)).toEqual(["f1", "end"]);
        expect(next.edges).toEqual([{ from: "trigger", to: "f1", when: null }]);
    });

    it("refuses to remove the trigger sentinel", () => {
        const def = fixtureDef();
        const next = withRemovedNode(def, "trigger");
        expect(next).toEqual(def);
    });
});

describe("withRemovedEdge", () => {
    it("removes an edge by synthetic id", () => {
        const def = fixtureDef();
        // Edge id format: e-${index}-${from}-${to}.
        const next = withRemovedEdge(def, "e-1-f1-t1");
        expect(next.edges).toEqual([
            { from: "trigger", to: "f1", when: null },
            { from: "t1", to: "end", when: null },
        ]);
    });

    it("ignores unknown edge ids", () => {
        const def = fixtureDef();
        const next = withRemovedEdge(def, "e-99-x-y");
        expect(next.edges).toEqual(def.edges);
    });
});

describe("withAddedEdge", () => {
    it("appends a new edge with null when", () => {
        const def = fixtureDef();
        const next = withAddedEdge(def, "f1", "end");
        expect(next.edges).toContainEqual({ from: "f1", to: "end", when: null });
    });

    it("rejects self-loops", () => {
        const def = fixtureDef();
        const next = withAddedEdge(def, "f1", "f1");
        expect(next.edges).toEqual(def.edges);
    });

    it("rejects duplicates", () => {
        const def = fixtureDef();
        const next = withAddedEdge(def, "f1", "t1");
        expect(next.edges).toEqual(def.edges);
    });

    it("normalizes the synthetic trigger id to 'trigger'", () => {
        const def = fixtureDef();
        const next = withAddedEdge(def, "__trigger__", "end");
        expect(next.edges).toContainEqual({
            from: "trigger",
            to: "end",
            when: null,
        });
    });
});

describe("withRenamedNode", () => {
    it("renames the node and cascades to edges", () => {
        const def = fixtureDef();
        const next = withRenamedNode(def, "f1", "filter_main");
        expect(next.nodes.map((n) => n.id)).toContain("filter_main");
        expect(next.edges).toContainEqual({
            from: "trigger",
            to: "filter_main",
            when: null,
        });
        expect(next.edges).toContainEqual({
            from: "filter_main",
            to: "t1",
            when: null,
        });
    });
});

describe("withUpdatedNode", () => {
    it("replaces the matching node's config", () => {
        const def = fixtureDef();
        const updated: NodeDef = {
            id: "f1",
            kind: "Filter",
            config: { expr: "event.payload.zone == \"driveway\"" },
        };
        const next = withUpdatedNode(def, updated);
        const found = next.nodes.find((n) => n.id === "f1");
        expect((found?.config as { expr: string }).expr).toBe(
            'event.payload.zone == "driveway"',
        );
    });
});

describe("END sentinel protection + normalization", () => {
    it("refuses to remove the synthetic END sentinel", () => {
        const def = fixtureDef();
        // Both the synthetic visual id and the canonical "END"
        // string should be rejected.
        const next1 = withRemovedNode(def, "__end__");
        expect(next1).toEqual(def);
        const next2 = withRemovedNode(def, "END");
        expect(next2).toEqual(def);
    });

    it("normalizes edges-to-END-sentinel-node-id to the canonical 'END' string", () => {
        const def = fixtureDef();
        // Operator drags from t1's source handle to the END sentinel
        // (visual id `__end__`); the persisted edge must use the
        // canonical END_SENTINEL string the runtime expects.
        const next = withAddedEdge(def, "t1", "__end__");
        expect(next.edges).toContainEqual({
            from: "t1",
            to: "END",
            when: null,
        });
    });

    it("rejects self-loop into the END sentinel from END", () => {
        const def = fixtureDef();
        // Even via the synthetic id, dragging END onto itself
        // (impossible UX-wise, but defensive) is a no-op.
        const next = withAddedEdge(def, "__end__", "END");
        expect(next).toEqual(def);
    });

    it("does NOT protect operator-created Terminal nodes named 'end'", () => {
        // Backward-compat: the protection is on the SENTINEL ids
        // (`__end__` / `"END"`), not on every node whose id happens
        // to be "end". Operators who drop a Terminal node and name
        // it "end" can still delete it like any other node.
        const def = fixtureDef(); // has node id "end" (Terminal kind)
        const next = withRemovedNode(def, "end");
        expect(next.nodes.map((n) => n.id)).toEqual(["f1", "t1"]);
        expect(next.edges).toEqual([
            { from: "trigger", to: "f1", when: null },
            { from: "f1", to: "t1", when: null },
        ]);
    });
});
