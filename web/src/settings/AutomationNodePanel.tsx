// Side panel that opens when the operator clicks a node on the canvas
// (M5 canvas-editor v2). Renders a kind-specific form: Filter →
// Rhai bool expression; Transform → Rhai value expression; Branch →
// notes (the actual branching lives on edges); Terminal → notes;
// AskAgent → prompt + attachments + exit tools.
//
// All forms write back through `onChange(updatedNode)` which the
// canvas applies to the AutomationDef. The panel is controlled —
// changes are visible immediately; no separate "save" inside the
// panel (the page's top-bar Save button persists the full def).

import { useState, type ChangeEvent } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import type { AutomationDef, ExitToolDef, NodeDef } from "../api/automations";

interface Props {
    /** The node being edited. `null` closes the panel. */
    node: NodeDef | null;
    /** The full definition — used so the panel can show e.g. the
     *  set of valid edge targets in a future Branch editor. */
    definition: AutomationDef;
    /** Replace the node in the definition; canvas owner re-renders. */
    onChange: (updated: NodeDef) => void;
    /** Rename the node id (cascades to edges). The canvas handles
     *  edge rewiring; we just emit the new id. */
    onRename: (oldId: string, newId: string) => void;
    /** Delete the node from the def. Canvas closes the panel. */
    onDelete: (id: string) => void;
    /** Close the panel without other side effects. */
    onClose: () => void;
}

export function AutomationNodePanel({
    node,
    definition,
    onChange,
    onRename,
    onDelete,
    onClose,
}: Props) {
    if (!node) return null;
    return (
        <div
            className="execlaw-automation-node-panel border rounded shadow-sm bg-white"
            style={{
                position: "absolute",
                top: 12,
                right: 12,
                width: 380,
                maxHeight: "calc(100% - 24px)",
                overflowY: "auto",
                zIndex: 10,
                padding: 12,
            }}
            data-testid="node-panel"
            onKeyDown={(e) => e.stopPropagation()}
        >
            <div className="d-flex justify-content-between align-items-start mb-2">
                <div>
                    <div className="text-muted small">{node.kind}</div>
                    <div className="h6 mb-0">{node.id}</div>
                </div>
                <Button
                    variant="link"
                    size="sm"
                    onClick={onClose}
                    aria-label="Close panel"
                    data-testid="node-panel-close"
                >
                    <i className="bi bi-x-lg" aria-hidden />
                </Button>
            </div>

            <RenameInput
                currentId={node.id}
                definition={definition}
                onRename={onRename}
            />

            <KindForm node={node} onChange={onChange} />

            <hr className="my-3" />
            <Button
                variant="outline-danger"
                size="sm"
                onClick={() => onDelete(node.id)}
                data-testid="node-panel-delete"
            >
                <i className="bi bi-trash me-1" aria-hidden />
                Delete node
            </Button>
        </div>
    );
}

function RenameInput({
    currentId,
    definition,
    onRename,
}: {
    currentId: string;
    definition: AutomationDef;
    onRename: (oldId: string, newId: string) => void;
}) {
    const [draft, setDraft] = useState(currentId);
    const [err, setErr] = useState<string | null>(null);
    const commit = () => {
        setErr(null);
        const trimmed = draft.trim();
        if (trimmed === currentId) return;
        if (trimmed === "") {
            setErr("Id cannot be empty");
            return;
        }
        if (trimmed === "trigger" || trimmed === "END") {
            setErr("Reserved id — pick another");
            return;
        }
        const taken = definition.nodes.some((n) => n.id === trimmed && n.id !== currentId);
        if (taken) {
            setErr(`Another node already uses id "${trimmed}"`);
            return;
        }
        onRename(currentId, trimmed);
    };
    return (
        <Form.Group className="mb-2">
            <Form.Label className="small text-muted mb-1">Node id</Form.Label>
            <div className="d-flex gap-2">
                <Form.Control
                    type="text"
                    size="sm"
                    value={draft}
                    onChange={(e: ChangeEvent<HTMLInputElement>) => setDraft(e.target.value)}
                    onBlur={commit}
                    onKeyDown={(e) => {
                        if (e.key === "Enter") commit();
                    }}
                    data-testid="node-panel-id-input"
                />
            </div>
            {err && <div className="small text-danger mt-1">{err}</div>}
            <div className="small text-muted mt-1">
                Edges referencing this node update automatically.
            </div>
        </Form.Group>
    );
}

function KindForm({
    node,
    onChange,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const setConfig = (next: Record<string, unknown>) => {
        onChange({ ...node, config: next });
    };
    switch (node.kind) {
        case "Filter":
            return (
                <ExprField
                    label="Filter expression (Rhai bool)"
                    placeholder='event.payload.zone == "driveway"'
                    value={(cfg.expr as string | undefined) ?? ""}
                    onChange={(v) => setConfig({ ...cfg, expr: v })}
                    help="Falsy → run drops to `skipped`."
                    testId="node-panel-filter-expr"
                />
            );
        case "Transform":
            return (
                <ExprField
                    label="Transform expression (Rhai value)"
                    placeholder='#{ doubled: event.payload.n * 2 }'
                    value={(cfg.expr as string | undefined) ?? ""}
                    onChange={(v) => setConfig({ ...cfg, expr: v })}
                    help="Output lands under this node's id in state for downstream `{{node_id.field}}` references."
                    testId="node-panel-transform-expr"
                />
            );
        case "Branch":
            return (
                <div className="small text-muted" data-testid="node-panel-branch-help">
                    A Branch is a routing junction with no config of its own.
                    Add multiple outgoing edges and set their <code>when</code>
                    {" "}clauses to control where the flow goes.
                </div>
            );
        case "Terminal":
            return (
                <div className="small text-muted" data-testid="node-panel-terminal-help">
                    Terminal nodes end the run with status <code>success</code>.
                    They have no config and no outgoing edges.
                </div>
            );
        case "AskAgent":
            return <AskAgentForm node={node} onChange={onChange} />;
        case "Notify":
            return <NotifyForm node={node} onChange={onChange} />;
        case "CallPlugin":
            return <CallPluginForm node={node} onChange={onChange} />;
        default:
            return (
                <div className="small text-danger">
                    Editing <code>{node.kind}</code> nodes from the canvas isn't
                    supported yet. Switch to the Code view.
                </div>
            );
    }
}

function ExprField({
    label,
    placeholder,
    value,
    onChange,
    help,
    testId,
}: {
    label: string;
    placeholder: string;
    value: string;
    onChange: (v: string) => void;
    help: string;
    testId: string;
}) {
    return (
        <Form.Group className="mb-2">
            <Form.Label className="small text-muted mb-1">{label}</Form.Label>
            <Form.Control
                as="textarea"
                rows={3}
                value={value}
                onChange={(e: ChangeEvent<HTMLTextAreaElement>) => onChange(e.target.value)}
                placeholder={placeholder}
                spellCheck={false}
                className="font-monospace small"
                data-testid={testId}
            />
            <div className="small text-muted mt-1">{help}</div>
        </Form.Group>
    );
}

function AskAgentForm({
    node,
    onChange,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const prompt = (cfg.prompt as string | undefined) ?? "";
    const attachments = (cfg.attachments as string[] | undefined) ?? [];
    const exitTools = (cfg.exit_tools as ExitToolDef[] | undefined) ?? [];
    const maxTurns = (cfg.max_turns as number | undefined) ?? null;

    const update = (next: Record<string, unknown>) =>
        onChange({ ...node, config: { ...cfg, ...next } });

    return (
        <>
            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Prompt</Form.Label>
                <Form.Control
                    as="textarea"
                    rows={4}
                    value={prompt}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        update({ prompt: e.target.value })
                    }
                    placeholder="Look at the image. Call notify() if you see an animal, otherwise ignore()."
                    spellCheck={false}
                    className="small"
                    data-testid="node-panel-askagent-prompt"
                />
                <div className="small text-muted mt-1">
                    Supports <code>{`{{event.payload.x}}`}</code> templating.
                </div>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Attachments (one URL or template per line)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    rows={2}
                    value={attachments.join("\n")}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        update({
                            attachments: e.target.value
                                .split("\n")
                                .map((s) => s.trim())
                                .filter((s) => s !== ""),
                        })
                    }
                    placeholder="{{event.payload.image_url}}"
                    className="font-monospace small"
                    data-testid="node-panel-askagent-attachments"
                />
                <div className="small text-muted mt-1">
                    Non-empty attachments route via the Vision backend; empty
                    runs through Standard.
                </div>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Max turns</Form.Label>
                <Form.Control
                    type="number"
                    min={1}
                    max={10}
                    value={maxTurns ?? ""}
                    onChange={(e: ChangeEvent<HTMLInputElement>) =>
                        update({
                            max_turns: e.target.value === "" ? null : Number(e.target.value),
                        })
                    }
                    placeholder="3"
                    data-testid="node-panel-askagent-maxturns"
                />
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Exit tools</Form.Label>
                {exitTools.length === 0 && (
                    <div className="small text-warning" data-testid="askagent-no-exit-tools">
                        At least one exit tool is required.
                    </div>
                )}
                {exitTools.map((t, idx) => (
                    <ExitToolRow
                        key={idx}
                        tool={t}
                        onChange={(updated) => {
                            const next = [...exitTools];
                            next[idx] = updated;
                            update({ exit_tools: next });
                        }}
                        onRemove={() => {
                            const next = exitTools.filter((_, i) => i !== idx);
                            update({ exit_tools: next });
                        }}
                        testId={`exit-tool-${idx}`}
                    />
                ))}
                <Button
                    variant="outline-secondary"
                    size="sm"
                    onClick={() =>
                        update({
                            exit_tools: [
                                ...exitTools,
                                {
                                    name: `tool_${exitTools.length + 1}`,
                                    description: "",
                                    args_schema: { type: "object" },
                                },
                            ],
                        })
                    }
                    data-testid="node-panel-askagent-add-tool"
                >
                    <i className="bi bi-plus-lg me-1" aria-hidden /> Add exit tool
                </Button>
            </Form.Group>
        </>
    );
}

function ExitToolRow({
    tool,
    onChange,
    onRemove,
    testId,
}: {
    tool: ExitToolDef;
    onChange: (updated: ExitToolDef) => void;
    onRemove: () => void;
    testId: string;
}) {
    return (
        <div
            className="border rounded p-2 mb-2"
            style={{ background: "#fafafa" }}
            data-testid={testId}
        >
            <div className="d-flex gap-2 mb-1">
                <Form.Control
                    size="sm"
                    value={tool.name}
                    onChange={(e: ChangeEvent<HTMLInputElement>) =>
                        onChange({ ...tool, name: e.target.value })
                    }
                    placeholder="name (e.g. notify)"
                    className="font-monospace small"
                />
                <Button
                    variant="outline-danger"
                    size="sm"
                    onClick={onRemove}
                    aria-label="Remove tool"
                >
                    <i className="bi bi-trash" aria-hidden />
                </Button>
            </div>
            <Form.Control
                as="textarea"
                rows={2}
                size="sm"
                value={tool.description}
                onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                    onChange({ ...tool, description: e.target.value })
                }
                placeholder="Description shown to the agent"
                className="small"
            />
        </div>
    );
}

const SEVERITIES = ["Critical", "Error", "Warning", "Info"] as const;
type Severity = (typeof SEVERITIES)[number];

function NotifyForm({
    node,
    onChange,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const title = (cfg.title as string | undefined) ?? "";
    const detail = (cfg.detail as string | undefined) ?? "";
    const severity = ((cfg.severity as string | undefined) ?? "Warning") as Severity;
    const source = (cfg.source as string | undefined) ?? "";

    const update = (next: Record<string, unknown>) =>
        onChange({ ...node, config: { ...cfg, ...next } });

    return (
        <>
            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Title</Form.Label>
                <Form.Control
                    type="text"
                    size="sm"
                    value={title}
                    onChange={(e: ChangeEvent<HTMLInputElement>) =>
                        update({ title: e.target.value })
                    }
                    placeholder="Motion detected in {{event.payload.zone}}"
                    spellCheck={false}
                    className="small"
                    data-testid="node-panel-notify-title"
                />
                <div className="small text-muted mt-1">
                    Required. Supports <code>{`{{event.payload.x}}`}</code>{" "}
                    templating.
                </div>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Detail</Form.Label>
                <Form.Control
                    as="textarea"
                    rows={2}
                    size="sm"
                    value={detail}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        update({ detail: e.target.value })
                    }
                    placeholder="Longer description of the alert"
                    spellCheck={false}
                    className="small"
                    data-testid="node-panel-notify-detail"
                />
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Severity</Form.Label>
                <Form.Select
                    size="sm"
                    value={severity}
                    onChange={(e: ChangeEvent<HTMLSelectElement>) =>
                        update({ severity: e.target.value })
                    }
                    data-testid="node-panel-notify-severity"
                >
                    {SEVERITIES.map((s) => (
                        <option key={s} value={s}>
                            {s}
                        </option>
                    ))}
                </Form.Select>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Source (optional)
                </Form.Label>
                <Form.Control
                    type="text"
                    size="sm"
                    value={source}
                    onChange={(e: ChangeEvent<HTMLInputElement>) =>
                        update({ source: e.target.value })
                    }
                    placeholder="automation:ring-watch"
                    className="font-monospace small"
                    data-testid="node-panel-notify-source"
                />
                <div className="small text-muted mt-1">
                    Used for alert dedup. Defaults to{" "}
                    <code>automation:&lt;node_id&gt;</code> when blank.
                </div>
            </Form.Group>
        </>
    );
}

function CallPluginForm({
    node,
    onChange,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const tool = (cfg.tool as string | undefined) ?? "";
    const args = (cfg.args as Record<string, unknown> | undefined) ?? {};

    // We render args as JSON in a textarea so authors can edit a
    // free-shape object. On change we attempt a parse â€” invalid JSON
    // is shown but not committed, so the user can keep typing
    // without losing other fields.
    const [argsDraft, setArgsDraft] = useState(JSON.stringify(args, null, 2));
    const [argsErr, setArgsErr] = useState<string | null>(null);

    const update = (next: Record<string, unknown>) =>
        onChange({ ...node, config: { ...cfg, ...next } });

    const commitArgs = () => {
        try {
            const parsed = JSON.parse(argsDraft);
            if (
                parsed === null ||
                typeof parsed !== "object" ||
                Array.isArray(parsed)
            ) {
                setArgsErr("args must be a JSON object");
                return;
            }
            setArgsErr(null);
            update({ args: parsed });
        } catch (e) {
            setArgsErr((e as Error).message);
        }
    };

    return (
        <>
            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Tool</Form.Label>
                <Form.Control
                    type="text"
                    size="sm"
                    value={tool}
                    onChange={(e: ChangeEvent<HTMLInputElement>) =>
                        update({ tool: e.target.value })
                    }
                    placeholder="signal.send_message"
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="node-panel-callplugin-tool"
                />
                <div className="small text-muted mt-1">
                    Registered tool name (see the Tools admin page for the
                    full list).
                </div>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Args (JSON object)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    rows={5}
                    size="sm"
                    value={argsDraft}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        setArgsDraft(e.target.value)
                    }
                    onBlur={commitArgs}
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="node-panel-callplugin-args"
                />
                {argsErr && (
                    <div
                        className="small text-danger mt-1"
                        data-testid="node-panel-callplugin-args-error"
                    >
                        {argsErr}
                    </div>
                )}
                <div className="small text-muted mt-1">
                    String leaves support <code>{`{{event.payload.x}}`}</code>{" "}
                    templating at run time.
                </div>
            </Form.Group>
        </>
    );
}
