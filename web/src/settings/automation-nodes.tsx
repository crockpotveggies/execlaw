// Custom ReactFlow node components per `NodeKind` (M5 — canvas
// editor v2). One component per kind so each can render the right
// icon, color, and body-shape for its config. ReactFlow looks up the
// component by the `type` field on the Node it passes to us — we
// register the mapping in `automation-canvas.tsx` via `nodeTypes`.
//
// All node components receive `data` containing the node's body-text
// summary + selected-flag styling. The handle positions (top input,
// bottom output) follow the canvas's top-to-bottom flow.

import { Handle, Position, type NodeProps } from "@xyflow/react";

export type CanvasNodeData = {
    label: string;
    detail?: string;
    /** When true, the canvas has selected this node (for the side panel). */
    selected?: boolean;
    /** Marks the synthetic trigger/end sentinels so we hide handle pieces
     *  they don't need (trigger has no input; end has no output). */
    sentinel?: "trigger" | "end";
};

interface KindStyle {
    /** Tinted background — a translucent wash of the kind's accent
     *  color over the dark canvas surface. */
    bg: string;
    /** Border / left-rail / icon accent — the kind's "identity" hue
     *  drawn from the app theme palette. */
    border: string;
    icon: string; // bootstrap-icon class fragment, e.g. "bi-funnel"
}

// Color palette aligned with `web/src/styles/theme.scss` so tiles
// read cleanly on the dark canvas (no light pastel backgrounds, no
// black-on-white text). Backgrounds are an 18% alpha tint of the
// border hue over the elevated surface; borders use the theme
// accent tokens directly so they stay distinguishable from each
// other while still feeling on-brand.
const STYLES: Record<string, KindStyle> = {
    // $accent (controller blue)
    Trigger: { bg: "rgba(68, 147, 248, 0.18)", border: "#4493f8", icon: "bi-lightning-charge-fill" },
    // muted dark gray — the End sentinel reads as "structure", not
    // a content node. Distinct from Terminal so the operator can
    // tell them apart at a glance.
    End: { bg: "rgba(125, 133, 144, 0.10)", border: "#7d8590", icon: "bi-flag-fill" },
    // $warning (amber)
    Filter: { bg: "rgba(210, 153, 34, 0.18)", border: "#d29922", icon: "bi-funnel" },
    // $success (green)
    Transform: { bg: "rgba(63, 185, 80, 0.18)", border: "#3fb950", icon: "bi-arrow-left-right" },
    // muted purple — distinct from accent + info
    Branch: { bg: "rgba(163, 113, 247, 0.18)", border: "#a371f7", icon: "bi-signpost-split" },
    // $text-muted
    Terminal: { bg: "rgba(125, 133, 144, 0.18)", border: "#7d8590", icon: "bi-stop-circle" },
    // orange — agent identity color, kept distinct from amber Filter
    AskAgent: { bg: "rgba(255, 138, 56, 0.18)", border: "#ff8a38", icon: "bi-robot" },
    // $danger (red) — alarm tile
    Notify: { bg: "rgba(248, 81, 73, 0.18)", border: "#f85149", icon: "bi-bell-fill" },
    // $info (lighter blue) — sits beside Trigger without conflicting
    CallPlugin: { bg: "rgba(88, 166, 255, 0.18)", border: "#58a6ff", icon: "bi-puzzle" },
    // teal — distinct outbound-reply identity, sits between Transform's
    // green and Trigger's blue without overlapping either
    SendReply: { bg: "rgba(45, 212, 191, 0.18)", border: "#2dd4bf", icon: "bi-reply-fill" },
};

function nodeShellStyle(
    kind: keyof typeof STYLES,
    selected: boolean,
): React.CSSProperties {
    const s = STYLES[kind] ?? STYLES.Terminal;
    return {
        background: s.bg,
        border: `${selected ? "2px" : "1px"} solid ${s.border}`,
        borderRadius: 8,
        padding: "8px 10px",
        minWidth: 160,
        maxWidth: 220,
        fontSize: 12,
        color: "#e6edf3", // $text-primary
        // Outer ring on selection (translucent accent halo); subtle
        // depth otherwise.
        boxShadow: selected
            ? `0 0 0 3px ${s.border}55`
            : "0 1px 2px rgba(0, 0, 0, 0.35)",
        // `grab` makes the affordance legible — React Flow flips this
        // to `grabbing` during the drag via its own classes.
        cursor: "grab",
    };
}

function NodeHeader({ kind, label }: { kind: keyof typeof STYLES; label: string }) {
    return (
        <div
            className="d-flex align-items-center mb-1"
            style={{ fontWeight: 600, color: "#e6edf3" }}
        >
            <i
                className={`bi ${STYLES[kind].icon} me-2`}
                style={{ color: STYLES[kind].border }}
                aria-hidden
            />
            <span style={{ flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                {label}
            </span>
        </div>
    );
}

function NodeDetail({ detail }: { detail?: string }) {
    if (!detail) return null;
    return (
        <div
            className="font-monospace small"
            style={{
                whiteSpace: "pre-wrap",
                wordBreak: "break-word",
                maxHeight: 60,
                overflow: "hidden",
                color: "#9aa6b2", // muted, but readable on dark tint
            }}
        >
            {detail}
        </div>
    );
}

export function TriggerNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("Trigger", !!data.selected)}>
            <NodeHeader kind="Trigger" label={data.label} />
            <NodeDetail detail={data.detail} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

/// The flow's synthetic terminal sentinel. Like the Trigger it's
/// not in `def.nodes` — operators can't delete or rename it; they
/// route their final node's outgoing edge into the End sentinel and
/// the runtime treats that as the flow's success terminus.
export function EndNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("End", !!data.selected)}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="End" label={data.label} />
            <NodeDetail detail={data.detail} />
            {/* No source handle — the flow stops here. */}
        </div>
    );
}

export function FilterNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("Filter", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="Filter" label={data.label} />
            <NodeDetail detail={data.detail} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

export function TransformNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("Transform", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="Transform" label={data.label} />
            <NodeDetail detail={data.detail} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

export function BranchNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("Branch", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="Branch" label={data.label} />
            <NodeDetail detail={data.detail ?? "Routes by edge `when` clauses"} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

export function TerminalNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("Terminal", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="Terminal" label={data.label} />
            <NodeDetail detail={data.detail ?? "Run ends here"} />
            {/* No source handle: terminal has no outgoing edges by design. */}
        </div>
    );
}

export function AskAgentNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("AskAgent", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="AskAgent" label={data.label} />
            <NodeDetail detail={data.detail} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

export function NotifyNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("Notify", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="Notify" label={data.label} />
            <NodeDetail detail={data.detail} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

export function CallPluginNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("CallPlugin", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="CallPlugin" label={data.label} />
            <NodeDetail detail={data.detail} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

export function SendReplyNode(props: NodeProps) {
    const data = props.data as unknown as CanvasNodeData;
    return (
        <div style={nodeShellStyle("SendReply", !!data.selected)} data-testid={`node-${props.id}`}>
            <Handle type="target" position={Position.Top} />
            <NodeHeader kind="SendReply" label={data.label} />
            <NodeDetail detail={data.detail ?? "Reply via envelope.origin"} />
            <Handle type="source" position={Position.Bottom} />
        </div>
    );
}

/** Map ReactFlow `type` string → React component for `nodeTypes`. */
export const NODE_TYPES = {
    Trigger: TriggerNode,
    End: EndNode,
    Filter: FilterNode,
    Transform: TransformNode,
    Branch: BranchNode,
    Terminal: TerminalNode,
    AskAgent: AskAgentNode,
    Notify: NotifyNode,
    CallPlugin: CallPluginNode,
    SendReply: SendReplyNode,
} as const;

export const KIND_ICONS: Record<string, string> = {
    Filter: STYLES.Filter.icon,
    Transform: STYLES.Transform.icon,
    Branch: STYLES.Branch.icon,
    Terminal: STYLES.Terminal.icon,
    AskAgent: STYLES.AskAgent.icon,
    Notify: STYLES.Notify.icon,
    CallPlugin: STYLES.CallPlugin.icon,
    SendReply: STYLES.SendReply.icon,
};

export const KIND_COLORS: Record<string, string> = {
    Filter: STYLES.Filter.border,
    Transform: STYLES.Transform.border,
    Branch: STYLES.Branch.border,
    Terminal: STYLES.Terminal.border,
    AskAgent: STYLES.AskAgent.border,
    Notify: STYLES.Notify.border,
    CallPlugin: STYLES.CallPlugin.border,
    SendReply: STYLES.SendReply.border,
};
