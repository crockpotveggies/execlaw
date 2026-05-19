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
    bg: string;
    border: string;
    icon: string; // bootstrap-icon class fragment, e.g. "bi-funnel"
}

const STYLES: Record<string, KindStyle> = {
    Trigger: { bg: "#e7f1ff", border: "#4a8cff", icon: "bi-lightning-charge-fill" },
    Filter: { bg: "#fef9c3", border: "#ca8a04", icon: "bi-funnel" },
    Transform: { bg: "#dcfce7", border: "#16a34a", icon: "bi-arrow-left-right" },
    Branch: { bg: "#ede9fe", border: "#7c3aed", icon: "bi-signpost-split" },
    Terminal: { bg: "#f3f4f6", border: "#6b7280", icon: "bi-stop-circle" },
    AskAgent: { bg: "#ffedd5", border: "#ea580c", icon: "bi-robot" },
    Notify: { bg: "#fee2e2", border: "#dc2626", icon: "bi-bell-fill" },
    CallPlugin: { bg: "#dbeafe", border: "#2563eb", icon: "bi-puzzle" },
};

function nodeShellStyle(
    kind: keyof typeof STYLES,
    selected: boolean,
): React.CSSProperties {
    const s = STYLES[kind] ?? STYLES.Terminal;
    return {
        background: s.bg,
        border: `${selected ? "2px" : "1px"} solid ${s.border}`,
        borderRadius: 6,
        padding: "8px 10px",
        minWidth: 160,
        maxWidth: 220,
        fontSize: 12,
        boxShadow: selected ? `0 0 0 3px ${s.border}33` : "0 1px 2px rgba(0,0,0,0.05)",
        cursor: "pointer",
    };
}

function NodeHeader({ kind, label }: { kind: keyof typeof STYLES; label: string }) {
    return (
        <div
            className="d-flex align-items-center mb-1"
            style={{ fontWeight: 600 }}
        >
            <i className={`bi ${STYLES[kind].icon} me-2`} aria-hidden />
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
            className="font-monospace small text-muted"
            style={{ whiteSpace: "pre-wrap", wordBreak: "break-word", maxHeight: 60, overflow: "hidden" }}
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

/** Map ReactFlow `type` string → React component for `nodeTypes`. */
export const NODE_TYPES = {
    Trigger: TriggerNode,
    Filter: FilterNode,
    Transform: TransformNode,
    Branch: BranchNode,
    Terminal: TerminalNode,
    AskAgent: AskAgentNode,
    Notify: NotifyNode,
    CallPlugin: CallPluginNode,
} as const;

export const KIND_ICONS: Record<string, string> = {
    Filter: STYLES.Filter.icon,
    Transform: STYLES.Transform.icon,
    Branch: STYLES.Branch.icon,
    Terminal: STYLES.Terminal.icon,
    AskAgent: STYLES.AskAgent.icon,
    Notify: STYLES.Notify.icon,
    CallPlugin: STYLES.CallPlugin.icon,
};

export const KIND_COLORS: Record<string, string> = {
    Filter: STYLES.Filter.border,
    Transform: STYLES.Transform.border,
    Branch: STYLES.Branch.border,
    Terminal: STYLES.Terminal.border,
    AskAgent: STYLES.AskAgent.border,
    Notify: STYLES.Notify.border,
    CallPlugin: STYLES.CallPlugin.border,
};
