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
    ReactFlow,
    ReactFlowProvider,
    useEdgesState,
    useNodesState,
    useReactFlow,
    type Connection,
    type Edge,
    type Node,
    type NodeChange,
    type EdgeChange,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import OverlayTrigger from "react-bootstrap/OverlayTrigger";
import Tooltip from "react-bootstrap/Tooltip";
import {
    AutomationNodePanel,
    EdgePanel,
    TriggerPanel,
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
    TriggerDef,
} from "../api/automations";

const TRIGGER_NODE_ID = "__trigger__";
/// Synthetic terminal sentinel. Mirrors `__trigger__` — it's not in
/// `def.nodes`, it represents the END_SENTINEL string ("END") that
/// edges use as their `to`. Undeletable, draggable for layout only.
/// Operators can still drop explicit `Terminal` nodes from the
/// palette for branch-specific terminations (e.g., "this branch
/// ends here without reaching the global end").
const END_NODE_ID = "__end__";
const END_SENTINEL = "END";
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

/// Position for the END sentinel — one rank below the lowest node
/// reachable from the trigger. Defaults to two ranks below the
/// trigger on empty flows so the sentinel doesn't overlap.
function defaultPositionForEnd(def: AutomationDef): { x: number; y: number } {
    // Reuse the BFS in `defaultPosition` by computing ranks the
    // same way for every operator node, then placing END one rank
    // below the max.
    const ranks = new Map<string, number>();
    ranks.set("trigger", 0);
    const queue: Array<{ id: string; depth: number }> = [
        { id: "trigger", depth: 0 },
    ];
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
    const maxRank = Math.max(0, ...ranks.values());
    return { x: X_CENTER, y: (maxRank + 1) * ROW_HEIGHT };
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
        case "RewritePrompt":
            return shorten(cfg.expr as string | undefined);
        case "SetSkills": {
            const skills = (cfg.skills as string[] | undefined) ?? [];
            return shorten(`+ ${skills.length} skill${skills.length === 1 ? "" : "s"}: ${skills.join(", ")}`);
        }
        case "SetTools": {
            const tools = (cfg.tools as string[] | undefined) ?? [];
            return shorten(`+ ${tools.length} tool${tools.length === 1 ? "" : "s"}: ${tools.join(", ")}`);
        }
        case "SetTrust":
            return shorten(`trust = ${(cfg.trust as string | undefined) ?? "?"}`);
        case "AddAttachment": {
            const ids = (cfg.attachment_ids as string[] | undefined) ?? [];
            return shorten(`+ ${ids.length} attachment${ids.length === 1 ? "" : "s"}`);
        }
        case "AddMemory":
            return shorten((cfg.text as string | undefined) ?? "(empty memory)");
        case "AskAgent":
            return shorten(cfg.prompt as string | undefined);
        case "Notify": {
            const title = (cfg.title as string | undefined) ?? "";
            const sev = (cfg.severity as string | undefined) ?? "Warning";
            return shorten(`[${sev}] ${title}`);
        }
        case "CallPlugin":
            return shorten((cfg.tool as string | undefined) ?? "");
        case "SendReply": {
            const source = (cfg.source as string | undefined) ?? "from_agent";
            if (source === "template") {
                return shorten((cfg.text as string | undefined) ?? "(empty template)");
            }
            const fromNode = (cfg.from_node as string | undefined) ?? "(upstream agent)";
            return shorten(`${source} ← ${fromNode}`);
        }
        default:
            return undefined;
    }
}

function buildGraph(
    def: AutomationDef,
    selectedNodeId: string | null,
    triggerSelected: boolean,
    selectedEdgeId: string | null = null,
): { nodes: Node[]; edges: Edge[] } {
    const triggerNode: Node = {
        id: TRIGGER_NODE_ID,
        position: def.nodes.find((n) => n.id === "trigger")?.position ??
            defaultPosition(def, "trigger"),
        data: {
            label: def.trigger.kind,
            detail: def.trigger.when ?? undefined,
            sentinel: "trigger",
            selected: triggerSelected,
        } satisfies CanvasNodeData,
        type: "Trigger",
        draggable: true,
        selected: triggerSelected,
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
    // The END sentinel — synthetic, undeletable, mirrors the trigger
    // sentinel at the bottom of the flow. Its position defaults to
    // a row just below the last operator node so simple linear flows
    // read top-to-bottom.
    const endNode: Node = {
        id: END_NODE_ID,
        position: defaultPositionForEnd(def),
        data: {
            label: "End",
            detail: "Flow ends successfully here",
            sentinel: "end",
        } satisfies CanvasNodeData,
        type: "End",
        draggable: true,
    };
    const nodes: Node[] = [triggerNode, ...typedNodes, endNode];

    const edges: Edge[] = def.edges.map((e, i) => {
        const id = `e-${i}-${e.from}-${e.to}`;
        const isSelected = id === selectedEdgeId;
        return {
            id,
            source: e.from === "trigger" ? TRIGGER_NODE_ID : e.from,
            target: e.to === END_SENTINEL ? END_NODE_ID : e.to,
            label: e.when ?? undefined,
            // Edge labels: light text on a dark pill so `when` clauses
            // stay readable on the dark canvas.
            labelStyle: { fontSize: 11, fill: "#e6edf3" },
            labelBgStyle: { fill: "#1f2630", fillOpacity: 0.85 },
            // Stroke uses `$text-muted` so edges are visible but don't
            // outshout the node tiles. Selected edges adopt the
            // controller-blue accent so the operator sees which edge
            // the side-panel applies to.
            style: {
                stroke: isSelected ? "#4493f8" : "#7d8590",
                strokeWidth: isSelected ? 2.5 : 1.5,
            },
            selected: isSelected,
            animated: e.when !== null && e.when !== undefined,
        };
    });
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
    // Both sentinels — trigger and end — are structural; they
    // can't be removed from a flow because every flow has a
    // trigger and an end.
    if (id === TRIGGER_NODE_ID || id === "trigger") return def;
    if (id === END_NODE_ID || id === END_SENTINEL) return def;
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

/// Replace an edge's `when` clause by canvas-edge-id. Used by the
/// EdgePanel for audit fix #3. `whenExpr` empty/whitespace -> stored
/// as `null` (unconditional). The id is the canvas-side
/// `e-{index}-{from}-{to}` synthetic id so the lookup is just an
/// index match. */
export function withUpdatedEdge(
    def: AutomationDef,
    edgeId: string,
    whenExpr: string | null,
): AutomationDef {
    const idx = def.edges.findIndex((e, i) => `e-${i}-${e.from}-${e.to}` === edgeId);
    if (idx < 0) return def;
    const trimmed = whenExpr?.trim() ?? "";
    const nextWhen = trimmed === "" ? null : whenExpr;
    const next = [...def.edges];
    next[idx] = { ...next[idx], when: nextWhen };
    return { ...def, edges: next };
}

export function withAddedEdge(
    def: AutomationDef,
    from: string,
    to: string,
): AutomationDef {
    const normalizedFrom = from === TRIGGER_NODE_ID ? "trigger" : from;
    // Edges that target the synthetic END sentinel persist as
    // `to: "END"` — the runtime's END_SENTINEL constant.
    const normalizedTo = to === END_NODE_ID ? END_SENTINEL : to;
    // The END sentinel has no outgoing edges by design — no source
    // handle in the UI. Defensive guard for imports / malformed
    // calls.
    if (
        normalizedFrom === END_SENTINEL ||
        normalizedFrom === END_NODE_ID ||
        from === END_NODE_ID
    ) {
        return def;
    }
    // Reject duplicates and self-loops.
    if (normalizedFrom === normalizedTo) return def;
    if (
        def.edges.some(
            (e) => e.from === normalizedFrom && e.to === normalizedTo,
        )
    ) {
        return def;
    }
    return {
        ...def,
        edges: [
            ...def.edges,
            { from: normalizedFrom, to: normalizedTo, when: null },
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

/** Replace the trigger block. Pass `when: ""` and we coerce to `null`
 *  so empty-textarea state doesn't get saved as a literal empty Rhai
 *  expression (which would always evaluate to a parser error and
 *  silently drop every event). */
export function withUpdatedTrigger(
    def: AutomationDef,
    updated: TriggerDef,
): AutomationDef {
    const when = updated.when?.trim() ? updated.when : null;
    return { ...def, trigger: { ...updated, when } };
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
        case "RewritePrompt":
            // Sensible starter: echo the user's text untouched. The
            // operator overwrites the Rhai with their own logic.
            return { expr: "event.payload.text" };
        case "SetSkills":
            return { skills: [] };
        case "SetTools":
            return { tools: [] };
        case "SetTrust":
            return { trust: "known_limited" };
        case "AddAttachment":
            return { attachment_ids: [] };
        case "AddMemory":
            return { text: "" };
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
        case "Notify":
            return {
                title: "Alert",
                detail: "",
                severity: "Warning",
            };
        case "CallPlugin":
            return {
                tool: "",
                args: {},
            };
        case "SendReply":
            return {
                source: "from_agent",
                from_node: "",
            };
        default:
            return {};
    }
}

function CanvasInner({ definition, onChange }: Props) {
    const reactFlow = useReactFlow();
    const wrapperRef = useRef<HTMLDivElement | null>(null);
    const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
    // The trigger sentinel and operator nodes share the click->panel
    // mechanic, but trigger has its own panel (kind dropdown + when
    // editor) and isn't in `def.nodes`. Tracked separately so a single
    // boolean tells us "is the trigger panel open?".
    const [triggerSelected, setTriggerSelected] = useState(false);
    // Canvas-edge-id (the synthetic `e-{i}-{from}-{to}` string) of the
    // edge whose side panel is open. Null means no edge selected.
    // Mutually exclusive with selectedNodeId / triggerSelected — the
    // click handlers clear the other two when an edge is selected.
    const [selectedEdgeId, setSelectedEdgeId] = useState<string | null>(null);
    const editable = !!onChange;

    // Visual node/edge state owned by ReactFlow. We let the lib drive
    // intermediate position updates during a drag (so the tile actually
    // follows the cursor instead of snapping back on release), and only
    // sync to the canonical `definition` once the drag ends or the
    // user otherwise mutates the graph. The `useEffect` below re-seeds
    // visuals when the parent's `definition` changes for an external
    // reason (palette drop, delete, rename, save-and-reload).
    const initial = useMemo(
        () => buildGraph(definition, selectedNodeId, triggerSelected, selectedEdgeId),
        // eslint-disable-next-line react-hooks/exhaustive-deps
        [],
    );
    const [nodes, setNodes, onNodesChangeXY] = useNodesState(initial.nodes);
    const [edges, setEdges, onEdgesChangeXY] = useEdgesState(initial.edges);

    // Re-seed visuals when `definition` changes for a reason other
    // than an in-flight drag (the dragstop branch in `onNodesChange`
    // calls `onChange`, but by then ReactFlow's internal position is
    // already where we want it, so the re-seed is a no-op visually).
    useEffect(() => {
        const built = buildGraph(
            definition,
            selectedNodeId,
            triggerSelected,
            selectedEdgeId,
        );
        setNodes(built.nodes);
        setEdges(built.edges);
    }, [
        definition,
        selectedNodeId,
        triggerSelected,
        selectedEdgeId,
        setNodes,
        setEdges,
    ]);

    // Position updates: ReactFlow fires `position` changes during the
    // drag (intermediate, `dragging: true`) AND on dragstop
    // (`dragging: false`). We hand all changes back to the lib via
    // `onNodesChangeXY` so the tile follows the cursor, then persist
    // to the parent only on stop. Select / remove changes flow through
    // the lib too — same reason: the visual state should advance
    // immediately while we batch the canonical-def update.
    const onNodesChange = useCallback(
        (changes: NodeChange[]) => {
            onNodesChangeXY(changes);
            if (!onChange) return;
            for (const ch of changes) {
                if (ch.type === "position" && ch.dragging === false && ch.position) {
                    // Sentinels aren't in `def.nodes` so don't try to
                    // persist their positions (they'd get added back
                    // at the BFS default on next render anyway).
                    if (ch.id === END_NODE_ID) continue;
                    const nodeId = ch.id === TRIGGER_NODE_ID ? "trigger" : ch.id;
                    onChange(withUpdatedPosition(definition, nodeId, ch.position));
                }
                if (ch.type === "remove") {
                    // Sentinels reject deletion in `withRemovedNode`;
                    // skip the panel-state side effect too so the UI
                    // doesn't blink.
                    if (ch.id === TRIGGER_NODE_ID || ch.id === END_NODE_ID) continue;
                    onChange(withRemovedNode(definition, ch.id));
                    if (selectedNodeId === ch.id) setSelectedNodeId(null);
                }
                if (ch.type === "select") {
                    // END sentinel has no config; everyone else opens
                    // a panel — the trigger gets its own panel
                    // (kind + when), operator nodes get the per-kind
                    // panel. Selecting any node also clears edge
                    // selection so the panel slot stays single-tenant.
                    if (ch.selected && ch.id === END_NODE_ID) {
                        // no-op — End has no editable state.
                    } else if (ch.selected && ch.id === TRIGGER_NODE_ID) {
                        setTriggerSelected(true);
                        setSelectedNodeId(null);
                        setSelectedEdgeId(null);
                    } else if (ch.selected) {
                        setSelectedNodeId(ch.id);
                        setTriggerSelected(false);
                        setSelectedEdgeId(null);
                    } else if (!ch.selected && ch.id === TRIGGER_NODE_ID) {
                        setTriggerSelected(false);
                    } else if (!ch.selected && selectedNodeId === ch.id) {
                        setSelectedNodeId(null);
                    }
                }
            }
        },
        [definition, onChange, onNodesChangeXY, selectedNodeId],
    );

    const onEdgesChange = useCallback(
        (changes: EdgeChange[]) => {
            onEdgesChangeXY(changes);
            if (!onChange) return;
            for (const ch of changes) {
                if (ch.type === "remove") {
                    onChange(withRemovedEdge(definition, ch.id));
                    if (selectedEdgeId === ch.id) setSelectedEdgeId(null);
                }
                if (ch.type === "select") {
                    if (ch.selected) {
                        setSelectedEdgeId(ch.id);
                        // Edges, nodes, and the trigger sentinel are
                        // mutually exclusive in the panel slot — only
                        // one panel visible at a time.
                        setSelectedNodeId(null);
                        setTriggerSelected(false);
                    } else if (selectedEdgeId === ch.id) {
                        setSelectedEdgeId(null);
                    }
                }
            }
        },
        [definition, onChange, onEdgesChangeXY, selectedEdgeId],
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

    const onTriggerChange = useCallback(
        (updated: TriggerDef) => {
            if (!onChange) return;
            onChange(withUpdatedTrigger(definition, updated));
        },
        [definition, onChange],
    );

    const onEdgeWhenChange = useCallback(
        (edgeId: string, whenExpr: string | null) => {
            if (!onChange) return;
            onChange(withUpdatedEdge(definition, edgeId, whenExpr));
        },
        [definition, onChange],
    );

    /** The currently-selected edge resolved against `definition.edges`.
     *  Maps the canvas-edge-id back to the raw `EdgeDef` so the panel
     *  can show `from`, `to`, and the current `when`. */
    const selectedEdge = useMemo(() => {
        if (!selectedEdgeId) return null;
        for (let i = 0; i < definition.edges.length; i += 1) {
            const e = definition.edges[i];
            if (`e-${i}-${e.from}-${e.to}` === selectedEdgeId) {
                return { id: selectedEdgeId, def: e };
            }
        }
        return null;
    }, [definition, selectedEdgeId]);

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
            className="execlaw-automation-canvas"
            style={{
                width: "100%",
                // Fill the remaining viewport height below the page
                // chrome (top nav + name/save bar + view toggle). The
                // floor keeps the canvas usable on short windows.
                height: "calc(100vh - 280px)",
                minHeight: 480,
                position: "relative",
            }}
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
                {/* Zoom toolbar — vertical (default orientation),
                 *  top-left so it doesn't fight the draggable palette
                 *  at bottom-left. `showFitView` defaults to true and
                 *  renders the four-arrow icon; we drop the lock with
                 *  `showInteractive={false}`. */}
                <Controls
                    position="top-left"
                    orientation="vertical"
                    showInteractive={false}
                />
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
            {editable && triggerSelected && (
                <TriggerPanel
                    trigger={definition.trigger}
                    onChange={onTriggerChange}
                    onClose={() => setTriggerSelected(false)}
                />
            )}
            {editable && selectedEdge && (
                <EdgePanel
                    edgeId={selectedEdge.id}
                    edge={selectedEdge.def}
                    definition={definition}
                    onWhenChange={onEdgeWhenChange}
                    onClose={() => setSelectedEdgeId(null)}
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
    // Phase A + B mutators — pre-turn middleware that shapes the
    // chat handler's inputs before the turn driver runs.
    "RewritePrompt",
    "SetSkills",
    "SetTools",
    "SetTrust",
    "AddAttachment",
    "AddMemory",
    "Notify",
    "CallPlugin",
    // AskAgent + SendReply stay in the schema (saved flows
    // containing them still load) but they're hidden from the
    // palette pending the middleware redesign — operators
    // shouldn't be authoring new graphs against the dead executors.
];

function NodePalette() {
    return (
        <div
            style={{
                position: "absolute",
                bottom: 12,
                left: 12,
                // $bg-surface w/ subtle border + soft shadow — matches
                // the rest of the app's floating-panel chrome.
                background: "#161b22",
                border: "1px solid #30363d",
                borderRadius: 8,
                padding: 8,
                display: "flex",
                gap: 6,
                zIndex: 5,
                boxShadow: "0 4px 12px rgba(0, 0, 0, 0.45)",
                color: "#e6edf3",
            }}
            data-testid="node-palette"
        >
            <div className="small me-2 align-self-center" style={{ color: "#7d8590" }}>
                Drag:
            </div>
            {PALETTE_KINDS.map((kind) => (
                <PaletteTile key={kind} kind={kind} />
            ))}
        </div>
    );
}

/// Operator-facing one-line description per kind. Surfaced in the
/// palette tile's hover tooltip so an author who doesn't recognize a
/// kind learns what dropping it does without leaving the canvas.
const PALETTE_TOOLTIPS: Record<NodeKind, string> = {
    Filter:
        "Drops the run unless a Rhai bool expression evaluates truthy. Common gate: only proceed when the event payload matches some condition.",
    Transform:
        "Rewrites the event/state into a new shape via a Rhai value expression. Use to extract or compute fields before downstream nodes see them.",
    Branch:
        "Routing junction with no config. Add multiple outgoing edges and set each one's `when` clause to fan out the flow.",
    Terminal:
        "Ends the run with status `success`. Optional — flows without an explicit Terminal also end at implicit graph leaves.",
    RewritePrompt:
        "Pre-turn mutator: runs a Rhai expression that returns a string. The result replaces the user-facing prompt the chat turn driver sees. Use to inject context, normalize phrasing, or add system guidance.",
    SetSkills:
        "Pre-turn mutator: appends skill names to the chat turn's applied skills. Merged with the composer's `+` menu picks. The skill prepend resolver fails the turn if a name is unknown — so prefer Filter-gated SetSkills for conditional injection.",
    SetTools:
        "Pre-turn mutator: appends tool names to caller_caps. Additive only — to narrow tools, use SetTrust to downgrade the trust class instead.",
    SetTrust:
        "Pre-turn mutator: overrides the resolved sender_trust for this turn. Useful for demoting trust on prompt-injection patterns (engages spotlighting + planner/executor split) or promoting trust for vetted senders.",
    AddAttachment:
        "Pre-turn mutator: appends attachment IDs to the turn's persisted_attachments. IDs must already exist in state_attachments; missing rows are skipped at hydration without failing the turn.",
    AddMemory:
        "Pre-turn mutator: prepends a `<memory>...</memory>` block to the user message. Supports `{{event.payload.x}}` templating. Multiple AddMemory nodes accumulate.",
    AskAgent:
        "Invokes the LLM with a prompt + attachments + exit tools. The agent picks one exit tool to terminate the turn; downstream Branches route on that tool name.",
    Notify:
        "Inserts an alert row (Info / Warning / Error / Critical). Useful for surfacing flow outcomes in the alerts dropdown without sending a chat reply.",
    CallPlugin:
        "Calls a plugin-registered tool by name (e.g., `calendar.create_event`) with templated args. The tool's return value becomes the node's output.",
    SendReply:
        "Delivers a reply back through the trigger's `envelope.origin` — chat thread, WhatsApp/Signal, the operator Inbox, or none, depending on the origin.",
    // Reserved — these tiles aren't in the default palette but the
    // typechecker forces an entry for every NodeKind variant.
    AppendToChat: "(reserved — not yet implemented)",
    HttpFetch: "(reserved — not yet implemented)",
    AwaitApproval: "(reserved — not yet implemented)",
    CallAutomation: "(reserved — not yet implemented)",
    Parallel: "(reserved — not yet implemented)",
    Join: "(reserved — not yet implemented)",
};

function PaletteTile({ kind }: { kind: NodeKind }) {
    const accent = KIND_COLORS[kind] ?? "#7d8590";
    const description = PALETTE_TOOLTIPS[kind];
    const tile = (
        <div
            draggable
            onDragStart={(e) => {
                e.dataTransfer.setData("application/x-execlaw-kind", kind);
                e.dataTransfer.effectAllowed = "move";
            }}
            style={{
                border: `1px dashed ${accent}`,
                borderRadius: 4,
                padding: "4px 8px",
                fontSize: 11,
                cursor: "grab",
                background: "#1f2630", // $bg-elev — slight lift from $bg-surface palette
                color: "#e6edf3",
                userSelect: "none",
            }}
            data-testid={`palette-${kind}`}
        >
            <i
                className={`bi ${KIND_ICONS[kind] ?? "bi-square"} me-1`}
                style={{ color: accent }}
                aria-hidden
            />
            {kind}
        </div>
    );
    return (
        <OverlayTrigger
            placement="top"
            // Slight delay-show so the tooltip doesn't fight a drag
            // gesture (drag starts immediately on mousedown; tooltip
            // would otherwise flash for the cursor's first pixel of
            // travel toward the canvas).
            delay={{ show: 250, hide: 0 }}
            overlay={
                <Tooltip
                    id={`palette-tooltip-${kind}`}
                    data-testid={`palette-tooltip-${kind}`}
                    style={{ maxWidth: 280 }}
                >
                    <div style={{ fontWeight: 600, marginBottom: 2 }}>
                        <i
                            className={`bi ${KIND_ICONS[kind] ?? "bi-square"} me-1`}
                            style={{ color: accent }}
                            aria-hidden
                        />
                        {kind}
                    </div>
                    <div style={{ fontSize: 11, lineHeight: 1.4, textAlign: "left" }}>
                        {description}
                    </div>
                </Tooltip>
            }
        >
            {tile}
        </OverlayTrigger>
    );
}

export function AutomationCanvas(props: Props) {
    return (
        <ReactFlowProvider>
            <CanvasInner {...props} />
        </ReactFlowProvider>
    );
}
