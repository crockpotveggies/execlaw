// ReactFlow visualization of an `AutomationDef` (M4c).
//
// Renders the trigger as a synthetic node at the top, the typed nodes
// in a top-to-bottom layout, and the edges with optional `when`-clause
// labels. Nodes are draggable for visual rearrangement (positions are
// local to the canvas — not persisted; the JSON view remains the
// source of truth for the definition).
//
// Read-only display in this iteration: no add/remove/edit affordances
// on the canvas itself. Use the JSON view for structural edits. A
// future iteration can layer config-on-click and drag-to-create-edge
// onto this base.

import {
    Background,
    Controls,
    MiniMap,
    ReactFlow,
    type Edge,
    type Node,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useMemo } from "react";
import type { AutomationDef, NodeDef } from "../api/automations";

interface Props {
    definition: AutomationDef;
}

const TRIGGER_NODE_ID = "__trigger__";
const END_NODE_ID = "__end__";
const X_CENTER = 240;
const ROW_HEIGHT = 100;

/**
 * Build ReactFlow nodes + edges from our internal `AutomationDef`.
 *
 * Layout strategy: rank nodes by reachability from the trigger
 * (BFS from `trigger` through the edges), place each rank in its
 * own row. Stable across re-renders given the same definition —
 * the operator's drag-to-rearrange in the canvas doesn't persist,
 * but a save → reload returns to the deterministic layout.
 */
function buildGraph(def: AutomationDef): { nodes: Node[]; edges: Edge[] } {
    const ranks = new Map<string, number>();
    ranks.set("trigger", 0);
    // BFS to assign rank to each node based on shortest-path distance
    // from the trigger sentinel.
    const queue: Array<{ id: string; depth: number }> = [
        { id: "trigger", depth: 0 },
    ];
    while (queue.length > 0) {
        const { id, depth } = queue.shift()!;
        for (const e of def.edges) {
            if (e.from === id) {
                const target = e.to;
                if (!ranks.has(target) || ranks.get(target)! > depth + 1) {
                    ranks.set(target, depth + 1);
                    queue.push({ id: target, depth: depth + 1 });
                }
            }
        }
    }

    // Group node ids by rank for horizontal spreading within a row.
    const byRank: Map<number, string[]> = new Map();
    for (const [id, rank] of ranks.entries()) {
        const arr = byRank.get(rank) ?? [];
        arr.push(id);
        byRank.set(rank, arr);
    }
    // Any node without an in-edge from the trigger graph still needs
    // to render — pin orphans to a fallback rank below the deepest
    // reached so they're visible but visibly orphaned.
    const maxRank = Math.max(0, ...ranks.values());
    let orphanRank = maxRank + 1;
    for (const n of def.nodes) {
        if (!ranks.has(n.id)) {
            ranks.set(n.id, orphanRank);
            const arr = byRank.get(orphanRank) ?? [];
            arr.push(n.id);
            byRank.set(orphanRank, arr);
            orphanRank += 1;
        }
    }

    const positionFor = (id: string): { x: number; y: number } => {
        const rank = ranks.get(id) ?? 0;
        const row = byRank.get(rank) ?? [];
        const idx = row.indexOf(id);
        const offset = (idx - (row.length - 1) / 2) * 220;
        return { x: X_CENTER + offset, y: rank * ROW_HEIGHT };
    };

    const triggerNode: Node = {
        id: TRIGGER_NODE_ID,
        position: positionFor("trigger"),
        data: { label: `Trigger: ${def.trigger.kind}` },
        type: "input",
        style: triggerStyle,
    };

    const typedNodes: Node[] = def.nodes.map((n: NodeDef) => ({
        id: n.id,
        position: positionFor(n.id),
        data: { label: nodeLabel(n) },
        type: n.kind === "Terminal" ? "output" : "default",
        style: styleForKind(n.kind),
    }));

    // If any edge points at the END sentinel, render a synthetic END
    // node so the operator sees the flow terminating explicitly.
    const hasEndEdge = def.edges.some((e) => e.to === "END");
    const endNode: Node | null = hasEndEdge
        ? {
              id: END_NODE_ID,
              position: { x: X_CENTER, y: (maxRank + 1) * ROW_HEIGHT },
              data: { label: "END" },
              type: "output",
              style: endStyle,
          }
        : null;

    const edges: Edge[] = def.edges.map((e, i) => ({
        id: `e-${i}-${e.from}-${e.to}`,
        source: e.from === "trigger" ? TRIGGER_NODE_ID : e.from,
        target: e.to === "END" ? END_NODE_ID : e.to,
        label: e.when ?? undefined,
        labelStyle: { fontSize: 11, fill: "#444" },
        labelBgStyle: { fill: "#fff", fillOpacity: 0.85 },
        style: { stroke: "#888", strokeWidth: 1.5 },
        animated: e.when !== null && e.when !== undefined,
    }));

    const nodes: Node[] = [triggerNode, ...typedNodes];
    if (endNode) nodes.push(endNode);
    return { nodes, edges };
}

function nodeLabel(n: NodeDef): string {
    // Show a short, kind-specific label so the canvas reads at a
    // glance instead of just listing ids.
    const cfg = (n.config ?? {}) as Record<string, unknown>;
    switch (n.kind) {
        case "Filter":
            return `Filter\n${shorten(cfg.expr as string | undefined)}`;
        case "Transform":
            return `Transform\n${shorten(cfg.expr as string | undefined)}`;
        case "Branch":
            return `Branch\n${n.id}`;
        case "Terminal":
            return `Terminal\n${n.id}`;
        case "AskAgent": {
            const prompt = cfg.prompt as string | undefined;
            return `AskAgent\n${shorten(prompt)}`;
        }
        default:
            return `${n.kind}\n${n.id}`;
    }
}

function shorten(s: string | undefined): string {
    if (!s) return "(no expr)";
    const trimmed = s.trim();
    if (trimmed.length <= 36) return trimmed;
    return `${trimmed.slice(0, 33)}…`;
}

const triggerStyle = {
    background: "#e7f1ff",
    border: "1px solid #4a8cff",
    fontSize: 12,
    padding: 6,
    whiteSpace: "pre-line" as const,
};
const endStyle = {
    background: "#f3f4f6",
    border: "1px solid #6b7280",
    fontSize: 12,
    padding: 6,
};
function styleForKind(kind: NodeDef["kind"]) {
    const base = {
        fontSize: 12,
        padding: 6,
        whiteSpace: "pre-line" as const,
    };
    switch (kind) {
        case "Filter":
            return { ...base, background: "#fef9c3", border: "1px solid #ca8a04" };
        case "Transform":
            return { ...base, background: "#dcfce7", border: "1px solid #16a34a" };
        case "Branch":
            return { ...base, background: "#ede9fe", border: "1px solid #7c3aed" };
        case "Terminal":
            return { ...base, background: "#f3f4f6", border: "1px solid #6b7280" };
        case "AskAgent":
            return { ...base, background: "#ffedd5", border: "1px solid #ea580c" };
        default:
            return { ...base, background: "#fff", border: "1px dashed #999" };
    }
}

export function AutomationCanvas({ definition }: Props) {
    const { nodes, edges } = useMemo(() => buildGraph(definition), [definition]);
    return (
        <div
            style={{ width: "100%", height: "520px" }}
            data-testid="automation-canvas"
        >
            <ReactFlow nodes={nodes} edges={edges} fitView fitViewOptions={{ padding: 0.2 }}>
                <Background />
                <Controls position="bottom-left" />
                <MiniMap pannable zoomable />
            </ReactFlow>
        </div>
    );
}
