// ReactFlow editor for an `AutomationDef` (M5 canvas-editor v2).
//
// Controlled-mode wrapper: the parent owns the `AutomationDef` and
// passes a `onChange(def)` callback. Every canvas mutation —
// drag-to-reposition, drag-to-create-edge, click-to-edit-via-panel,
// delete-on-keyboard, drop-from-palette — flows through that single
// setter so the JSON view stays a faithful round-trip of the canvas.
//
// Node positions persist on `NodeDef.position`. Drags batched at
// dragstop so we don't fire 60+ updates per move.

import {
    Background,
    Controls,
    MiniMap,
    ReactFlow,
    ReactFlowProvider,
    useReactFlow,
    type Connection,
    type Edge,
    type Node,
    type NodeChange,
    type EdgeChange,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useCallback, useMemo, useRef, useState } from "react";
import {
    AutomationNodePanel,
} from "./AutomationNodePanel";
import {
    KIND_COLORS,
    KIND_ICONS,
    NODE_TYPES,
    type CanvasNodeData,
} from "./automation-nodes";
import type {
    AutomationDef,
    NodeDef,
    NodeKind,
} from "../api/automations";

const TRIGGER_NODE_ID = "__trigger__";
const X_CENTER = 240;
const ROW_HEIGHT = 100;

interface Props {
    definition: AutomationDef;
    /** When `null`, the canvas is read-only (no editing affordances).
     *  When set, every canvas mutation flows through this callback. */
    onChange?: ((next: AutomationDef) => void) | null;
}

/**
 * BFS-rank layout used as the *fallback* when a node has no
 * `position`. Once the operator drags the node, `NodeDef.position`
 * gets set and BFS is no longer consulted for it.
 */
function defaultPosition(
    def: AutomationDef,
    nodeId: string,
): { x: number; y: number } {
    const ranks = new Map<string, number>();
    ranks.set("trigger", 0);
    const queue: Array<{ id: string; depth: number }> = [{ id: "trigger", depth: 0 }];
    while (queue.length > 0) {
        const { id, depth } = queue.shift()!;
        for (const e of def.edges) {
            if (e.from === id) {
                if (!ranks.has(e.to) || ranks.get(e.to)! > depth + 1) {
                    ranks.set(e.to, depth + 1);
                    queue.push({ id: e.to, depth: depth + 1 });
                }
            }
        }
    }
    const byRank: Map<number, string[]> = new Map();
    for (const [id, rank] of ranks.entries()) {
        const arr = byRank.get(rank) ?? [];
        arr.push(id);
        byRank.set(rank, arr);
    }
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
    const rank = ranks.get(nodeId) ?? 0;
    const row = byRank.get(rank) ?? [];
    const idx = row.indexOf(nodeId);
    const offset = (idx - (row.length - 1) / 2) * 220;
    return { x: X_CENTER + offset, y: rank * ROW_HEIGHT };
}

function shorten(s: string | undefined): string {
    if (!s) return "";
    const trimmed = s.trim();
    if (trimmed.length <= 60) return trimmed;
    return `${trimmed.slice(0, 57)}…`;
}

function bodyFor(n: NodeDef): string | undefined {
    const cfg = (n.config ?? {}) as Record<string, unknown>;
    switch (n.kind) {
        case "Filter":
        case "Transform":
            return shorten(cfg.expr as string | undefined);
        case "AskAgent":
            return shorten(cfg.prompt as string | undefined);
        default:
            return undefined;
    }
}

function buildGraph(
    def: AutomationDef,
    selectedNodeId: string | null,
): { nodes: Node[]; edges: Edge[] } {
    const triggerNode: Node = {
        id: TRIGGER_NODE_ID,
        position: def.nodes.find((n) => n.id === "trigger")?.position ??
            defaultPosition(def, "trigger"),
        data: {
            label: def.trigger.kind,
            detail: def.trigger.when ?? undefined,
            sentinel: "trigger",
        } satisfies CanvasNodeData,
        type: "Trigger",
        draggable: true,
    };
    const typedNodes: Node[] = def.nodes.map((n) => ({
        id: n.id,
        position: n.position ?? defaultPosition(def, n.id),
        data: {
            label: n.id,
            detail: bodyFor(n),
            selected: selectedNodeId === n.id,
        } satisfies CanvasNodeData,
        type: n.kind,
        draggable: true,
        selected: selectedNodeId === n.id,
    }));
    const nodes: Node[] = [triggerNode, ...typedNodes];

    const edges: Edge[] = def.edges.map((e, i) => ({
        id: `e-${i}-${e.from}-${e.to}`,
        source: e.from === "trigger" ? TRIGGER_NODE_ID : e.from,
        target: e.to,
        label: e.when ?? undefined,
        labelStyle: { fontSize: 11, fill: "#444" },
        labelBgStyle: { fill: "#fff", fillOpacity: 0.85 },
        style: { stroke: "#888", strokeWidth: 1.5 },
        animated: e.when !== null && e.when !== undefined,
    }));
    return { nodes, edges };
}

export function withUpdatedPosition(
    def: AutomationDef,
    id: string,
    pos: { x: number; y: number },
): AutomationDef {
    return {
        ...def,
        nodes: def.nodes.map((n) =>
            n.id === id ? { ...n, position: { x: pos.x, y: pos.y } } : n,
        ),
    };
}

export function withRemovedNode(def: AutomationDef, id: string): AutomationDef {
    if (id === TRIGGER_NODE_ID || id === "trigger") return def;
    return {
        ...def,
        nodes: def.nodes.filter((n) => n.id !== id),
        edges: def.edges.filter((e) => e.from !== id && e.to !== id),
    };
}

export function withRemovedEdge(def: AutomationDef, edgeId: string): AutomationDef {
    return {
        ...def,
        edges: def.edges.filter((_, i) => {
            const e = def.edges[i];
            return `e-${i}-${e.from}-${e.to}` !== edgeId;
        }),
    };
}

export function withAddedEdge(
    def: AutomationDef,
    from: string,
    to: string,
): AutomationDef {
    const normalizedFrom = from === TRIGGER_NODE_ID ? "trigger" : from;
    // Reject duplicates and self-loops.
    if (normalizedFrom === to) return def;
    if (def.edges.some((e) => e.from === normalizedFrom && e.to === to)) {
        return def;
    }
    return {
        ...def,
        edges: [
            ...def.edges,
            { from: normalizedFrom, to, when: null },
        ],
    };
}

export function withRenamedNode(
    def: AutomationDef,
    oldId: string,
    newId: string,
): AutomationDef {
    return {
        ...def,
        nodes: def.nodes.map((n) => (n.id === oldId ? { ...n, id: newId } : n)),
        edges: def.edges.map((e) => ({
            ...e,
            from: e.from === oldId ? newId : e.from,
            to: e.to === oldId ? newId : e.to,
        })),
    };
}

export function withUpdatedNode(def: AutomationDef, updated: NodeDef): AutomationDef {
    return {
        ...def,
        nodes: def.nodes.map((n) => (n.id === updated.id ? updated : n)),
    };
}

function withAddedNode(def: AutomationDef, n: NodeDef): AutomationDef {
    return { ...def, nodes: [...def.nodes, n] };
}

function mintId(def: AutomationDef, kind: NodeKind): string {
    const prefix = kind.toLowerCase();
    let i = 1;
    while (def.nodes.some((n) => n.id === `${prefix}${i}`)) i += 1;
    return `${prefix}${i}`;
}

function defaultConfigFor(kind: NodeKind): unknown {
    switch (kind) {
        case "Filter":
            return { expr: "true" };
        case "Transform":
            return { expr: "#{}" };
        case "AskAgent":
            return {
                prompt: "Decide.",
                attachments: [],
                exit_tools: [
                    {
                        name: "ok",
                        description: "Default outcome",
                        args_schema: { type: "object" },
                    },
                ],
            };
        default:
            return {};
    }
}

function CanvasInner({ definition, onChange }: Props) {
    const reactFlow = useReactFlow();
    const wrapperRef = useRef<HTMLDivElement | null>(null);
    const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
    const editable = !!onChange;

    const { nodes, edges } = useMemo(
        () => buildGraph(definition, selectedNodeId),
        [definition, selectedNodeId],
    );

    // Position updates: ReactFlow fires `position` changes during the
    // drag (intermediate) AND on dragstop. We only persist on stop —
    // intermediate positions are visual-only.
    const onNodesChange = useCallback(
        (changes: NodeChange[]) => {
            if (!onChange) return;
            for (const ch of changes) {
                if (ch.type === "position" && ch.dragging === false && ch.position) {
                    const nodeId = ch.id === TRIGGER_NODE_ID ? "trigger" : ch.id;
                    onChange(withUpdatedPosition(definition, nodeId, ch.position));
                }
                if (ch.type === "remove") {
                    onChange(withRemovedNode(definition, ch.id));
                    if (selectedNodeId === ch.id) setSelectedNodeId(null);
                }
                if (ch.type === "select") {
                    if (ch.selected && ch.id !== TRIGGER_NODE_ID) {
                        setSelectedNodeId(ch.id);
                    } else if (!ch.selected && selectedNodeId === ch.id) {
                        setSelectedNodeId(null);
                    }
                }
            }
        },
        [definition, onChange, selectedNodeId],
    );

    const onEdgesChange = useCallback(
        (changes: EdgeChange[]) => {
            if (!onChange) return;
            for (const ch of changes) {
                if (ch.type === "remove") {
                    onChange(withRemovedEdge(definition, ch.id));
                }
            }
        },
        [definition, onChange],
    );

    const onConnect = useCallback(
        (params: Connection) => {
            if (!onChange) return;
            if (!params.source || !params.target) return;
            onChange(withAddedEdge(definition, params.source, params.target));
        },
        [definition, onChange],
    );

    const onDrop = useCallback(
        (e: React.DragEvent<HTMLDivElement>) => {
            if (!onChange) return;
            e.preventDefault();
            const kind = e.dataTransfer.getData("application/x-execlaw-kind") as
                | NodeKind
                | "";
            if (!kind) return;
            const wrapper = wrapperRef.current;
            if (!wrapper) return;
            const bounds = wrapper.getBoundingClientRect();
            const pos = reactFlow.screenToFlowPosition({
                x: e.clientX - bounds.left,
                y: e.clientY - bounds.top,
            });
            const id = mintId(definition, kind);
            const newNode: NodeDef = {
                id,
                kind,
                config: defaultConfigFor(kind) as Record<string, unknown>,
                position: { x: pos.x, y: pos.y },
            };
            onChange(withAddedNode(definition, newNode));
            setSelectedNodeId(id);
        },
        [definition, onChange, reactFlow],
    );

    const onDragOver = useCallback((e: React.DragEvent<HTMLDivElement>) => {
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
    }, []);

    const selectedNode = useMemo<NodeDef | null>(() => {
        if (!selectedNodeId) return null;
        return definition.nodes.find((n) => n.id === selectedNodeId) ?? null;
    }, [definition, selectedNodeId]);

    const onNodeChange = useCallback(
        (updated: NodeDef) => {
            if (!onChange) return;
            onChange(withUpdatedNode(definition, updated));
        },
        [definition, onChange],
    );

    const onRename = useCallback(
        (oldId: string, newId: string) => {
            if (!onChange) return;
            onChange(withRenamedNode(definition, oldId, newId));
            setSelectedNodeId(newId);
        },
        [definition, onChange],
    );

    const onDelete = useCallback(
        (id: string) => {
            if (!onChange) return;
            onChange(withRemovedNode(definition, id));
            setSelectedNodeId(null);
        },
        [definition, onChange],
    );

    return (
        <div
            ref={wrapperRef}
            style={{ width: "100%", height: "560px", position: "relative" }}
            data-testid="automation-canvas"
            onDrop={onDrop}
            onDragOver={onDragOver}
        >
            <ReactFlow
                nodes={nodes}
                edges={edges}
                nodeTypes={NODE_TYPES}
                onNodesChange={onNodesChange}
                onEdgesChange={onEdgesChange}
                onConnect={editable ? onConnect : undefined}
                nodesDraggable={editable}
                nodesConnectable={editable}
                elementsSelectable={editable}
                deleteKeyCode={editable ? ["Backspace", "Delete"] : null}
                fitView
                fitViewOptions={{ padding: 0.2 }}
            >
                <Background />
                <Controls position="bottom-left" showInteractive={false} />
                <MiniMap pannable zoomable />
            </ReactFlow>
            {editable && <NodePalette />}
            {editable && selectedNode && (
                <AutomationNodePanel
                    node={selectedNode}
                    definition={definition}
                    onChange={onNodeChange}
                    onRename={onRename}
                    onDelete={onDelete}
                    onClose={() => setSelectedNodeId(null)}
                />
            )}
        </div>
    );
}

const PALETTE_KINDS: NodeKind[] = [
    "Filter",
    "Transform",
    "Branch",
    "Terminal",
    "AskAgent",
];

function NodePalette() {
    return (
        <div
            style={{
                position: "absolute",
                bottom: 12,
                left: 12,
                background: "white",
                border: "1px solid #ddd",
                borderRadius: 8,
                padding: 8,
                display: "flex",
                gap: 6,
                zIndex: 5,
                boxShadow: "0 2px 6px rgba(0,0,0,0.08)",
            }}
            data-testid="node-palette"
        >
            <div className="small text-muted me-2 align-self-center">Drag:</div>
            {PALETTE_KINDS.map((kind) => (
                <PaletteTile key={kind} kind={kind} />
            ))}
        </div>
    );
}

function PaletteTile({ kind }: { kind: NodeKind }) {
    return (
        <div
            draggable
            onDragStart={(e) => {
                e.dataTransfer.setData("application/x-execlaw-kind", kind);
                e.dataTransfer.effectAllowed = "move";
            }}
            style={{
                border: `1px dashed ${KIND_COLORS[kind] ?? "#888"}`,
                borderRadius: 4,
                padding: "4px 8px",
                fontSize: 11,
                cursor: "grab",
                background: "#fafafa",
                userSelect: "none",
            }}
            data-testid={`palette-${kind}`}
            title={`Drag a ${kind} node onto the canvas`}
        >
            <i className={`bi ${KIND_ICONS[kind] ?? "bi-square"} me-1`} aria-hidden />
            {kind}
        </div>
    );
}

export function AutomationCanvas(props: Props) {
    return (
        <ReactFlowProvider>
            <CanvasInner {...props} />
        </ReactFlowProvider>
    );
}
