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

import { useEffect, useMemo, useState, type ChangeEvent } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import type {
    AutomationDef,
    BranchSuggestion,
    EdgeDef,
    ExitToolDef,
    NodeDef,
    RegisteredEventKind,
    TriggerDef,
} from "../api/automations";
import {
    filterBranchSuggestions,
    listBranchSuggestions,
    listRegisteredEvents,
    renderBranchSuggestion,
} from "../api/automations";
import { listSkills, listTools } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

/**
 * Slice G (2026-05-26) — fetch plugin-contributed branch suggestions
 * once per panel-mount. The endpoint walks every installed plugin's
 * `[[branch_suggestions]]` manifest section and returns the union
 * (typically a dozen-ish entries — light enough to skip caching).
 *
 * Returns `[]` until the fetch resolves, and on auth/network failure
 * — the picker is purely additive UX, so a missing fetch silently
 * falls back to the kind-specific built-in suggestions.
 */
function useBranchSuggestions(): BranchSuggestion[] {
    const auth = useAuth();
    const [out, setOut] = useState<BranchSuggestion[]>([]);
    useEffect(() => {
        let cancelled = false;
        listBranchSuggestions(auth.getAccessToken)
            .then((list) => {
                if (!cancelled) setOut(list);
            })
            .catch(() => {
                // Best-effort: surfacing a fetch error in the panel
                // would scare operators away from a useful flow-edit
                // session. Stale-fallback to built-in suggestions only.
            });
        return () => {
            cancelled = true;
        };
    }, [auth.getAccessToken]);
    return out;
}

/// Phase D (2026-05-22) — operator-pickable name lists for the
/// SetSkills / SetTools mutator forms. Returns just the names so
/// the form's chip strip stays simple; rich metadata stays out of
/// the panel.
///
/// `fetchActiveSkillNames` filters out archived skills (the
/// resolver rejects archived names with a 400 at turn time) and
/// returns whatever survives, alphabetically sorted for stable
/// rendering. Empty result is "no skills installed yet" — the
/// form's hint footer shows a friendly placeholder.
async function fetchActiveSkillNames(token: () => string | null): Promise<string[]> {
    const resp = await listSkills(token, { includeArchived: false });
    const names = resp.skills
        .filter((s) => s.state !== "archived")
        .map((s) => s.name);
    names.sort();
    return names;
}

/// `fetchEnabledToolNames` filters out disabled tools (calling a
/// disabled tool fails at dispatch time) and tools removed from
/// the registry. Alphabetical for stable rendering.
async function fetchEnabledToolNames(token: () => string | null): Promise<string[]> {
    const resp = await listTools(token);
    const names = resp.tools
        .filter((t) => t.enabled && t.removed_at === null)
        .map((t) => t.tool_name);
    names.sort();
    return names;
}

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
            className="execlaw-automation-node-panel border rounded shadow-sm"
            style={{
                position: "absolute",
                top: 12,
                right: 12,
                width: 380,
                maxHeight: "calc(100% - 24px)",
                overflowY: "auto",
                zIndex: 10,
                padding: 12,
                // Match the floating-panel chrome used elsewhere in
                // the app's dark theme ($bg-surface + $border-subtle).
                background: "#161b22",
                borderColor: "#30363d",
                color: "#e6edf3",
                boxShadow: "0 4px 12px rgba(0, 0, 0, 0.45)",
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
        case "RewritePrompt":
            return (
                <ExprField
                    label="Rewrite expression (Rhai value → string)"
                    placeholder='"[fix] " + event.payload.text'
                    value={(cfg.expr as string | undefined) ?? ""}
                    onChange={(v) => setConfig({ ...cfg, expr: v })}
                    help="Must return a string. The result replaces the user-facing prompt the chat turn driver sees. Errors (non-string result, bad Rhai) surface in the run trace and the chat falls back to the un-mutated input."
                    testId="node-panel-rewrite-prompt-expr"
                />
            );
        case "SetSkills":
            return <StringListForm
                node={node}
                onChange={onChange}
                fieldKey="skills"
                label="Skill names (one per line)"
                placeholder="calendar&#10;notes_taker"
                help="Appended to the chat turn's applied skills. Each name must match an entry in state_skills (the skill prepend resolver fails the turn otherwise — gate with Filter to be safe)."
                testId="node-panel-set-skills"
                hintFetcher={fetchActiveSkillNames}
                hintLabel="Installed skills (click to append)"
            />;
        case "SetTools":
            return <StringListForm
                node={node}
                onChange={onChange}
                fieldKey="tools"
                label="Tool names (one per line)"
                placeholder="python.execute&#10;web.search"
                help="Added to caller_caps for this turn. Additive only — use SetTrust to downgrade trust if you want a smaller tool surface."
                testId="node-panel-set-tools"
                hintFetcher={fetchEnabledToolNames}
                hintLabel="Registered tools (click to append)"
            />;
        case "SetTrust":
            return <SetTrustForm node={node} onChange={onChange} />;
        case "AddAttachment":
            return <StringListForm
                node={node}
                onChange={onChange}
                fieldKey="attachment_ids"
                label="Attachment IDs (one per line)"
                placeholder="att-abc-123&#10;att-def-456"
                help="Appended to the turn's persisted_attachments. IDs must already exist in state_attachments — missing rows skip silently at hydration."
                testId="node-panel-add-attachment"
            />;
        case "AddMemory":
            return (
                <Form.Group className="mb-2">
                    <Form.Label className="small text-muted mb-1">
                        Memory text
                    </Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={4}
                        value={(cfg.text as string | undefined) ?? ""}
                        onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                            setConfig({ ...cfg, text: e.target.value })
                        }
                        placeholder="Operator prefers concise replies. Always default to metric units."
                        spellCheck={false}
                        className="small"
                        data-testid="node-panel-add-memory-text"
                    />
                    <div className="small text-muted mt-1">
                        Prepended to the user message wrapped in
                        {" "}<code>&lt;memory&gt;...&lt;/memory&gt;</code>.
                        Supports <code>{`{{event.payload.x}}`}</code> templating.
                    </div>
                </Form.Group>
            );
        case "AskAgent":
            return <AskAgentForm node={node} onChange={onChange} />;
        case "Notify":
            return <NotifyForm node={node} onChange={onChange} />;
        case "CallPlugin":
            return <CallPluginForm node={node} onChange={onChange} />;
        case "SendReply":
            return <SendReplyForm node={node} onChange={onChange} />;
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
    const reasoningTools = (cfg.reasoning_tools as string[] | undefined) ?? [];

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
                <Form.Label className="small text-muted mb-1">
                    Reasoning tools (one tool name per line, optional)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    rows={2}
                    size="sm"
                    value={reasoningTools.join("\n")}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        update({
                            reasoning_tools: e.target.value
                                .split("\n")
                                .map((s) => s.trim())
                                .filter((s) => s !== ""),
                        })
                    }
                    placeholder="python.execute&#10;web.search"
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="node-panel-askagent-reasoning-tools"
                />
                <div className="small text-muted mt-1">
                    Optional palette the agent may call while reasoning,
                    before committing to an exit_tool. Must be a subset
                    of the trust profile's allowed_tools — out-of-palette
                    names are rejected at invoke time.
                </div>
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
            style={{
                // Slight lift from the panel's own $bg-surface so
                // exit-tool rows are visually grouped without being
                // bright against the dark canvas.
                background: "#1f2630",
                borderColor: "#30363d",
            }}
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

/// Phase B mutator helper — line-delimited textarea backed by a
/// JSON-array config field. Used by SetSkills / SetTools /
/// AddAttachment (all three share the shape: a single string-array
/// field with a different key name). Empty lines + whitespace-only
/// lines are dropped on commit so a trailing newline doesn't break
/// the validator.
///
/// Phase D (2026-05-22) extension: optional `hintFetcher` produces
/// a `string[]` of operator-pickable names rendered as clickable
/// chips below the textarea. Click a chip → append to the list
/// (deduped against entries already present). Used by SetSkills +
/// SetTools to surface the registered skill / tool catalog the
/// validator will check at execute time. Loading + error states
/// render explicitly so a slow / down admin endpoint doesn't
/// silently strand the operator.
function StringListForm({
    node,
    onChange,
    fieldKey,
    label,
    placeholder,
    help,
    testId,
    hintFetcher,
    hintLabel,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
    fieldKey: string;
    label: string;
    placeholder: string;
    help: string;
    testId: string;
    /** Optional. When set, renders a "Available:" hint chip strip
     *  below the textarea. Called once on mount with the current
     *  auth token. */
    hintFetcher?: (token: () => string | null) => Promise<string[]>;
    /** Plain-English label preceding the hint chips, e.g.
     *  "Installed skills" / "Registered tools". Required when
     *  `hintFetcher` is set. */
    hintLabel?: string;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const items = (cfg[fieldKey] as string[] | undefined) ?? [];

    // Hint chip state — only used when hintFetcher is set.
    const auth = useAuth();
    const [hints, setHints] = useState<string[] | null>(null);
    const [hintErr, setHintErr] = useState<string | null>(null);
    useEffect(() => {
        if (!hintFetcher) return;
        let cancelled = false;
        hintFetcher(auth.getAccessToken)
            .then((names) => {
                if (!cancelled) setHints(names);
            })
            .catch((e: Error) => {
                if (!cancelled) setHintErr(e.message);
            });
        return () => {
            cancelled = true;
        };
        // Effect runs once on mount; auth + hintFetcher are stable
        // for the lifetime of the panel. eslint-disable-next-line
        // react-hooks/exhaustive-deps
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    const appendName = (name: string) => {
        if (items.includes(name)) return;
        onChange({
            ...node,
            config: { ...cfg, [fieldKey]: [...items, name] },
        });
    };

    return (
        <Form.Group className="mb-2">
            <Form.Label className="small text-muted mb-1">{label}</Form.Label>
            <Form.Control
                as="textarea"
                rows={3}
                value={items.join("\n")}
                onChange={(e: ChangeEvent<HTMLTextAreaElement>) => {
                    const next = e.target.value
                        .split("\n")
                        .map((s) => s.trim())
                        .filter((s) => s !== "");
                    onChange({
                        ...node,
                        config: { ...cfg, [fieldKey]: next },
                    });
                }}
                placeholder={placeholder}
                spellCheck={false}
                className="font-monospace small"
                data-testid={`${testId}-textarea`}
            />
            <div className="small text-muted mt-1">{help}</div>
            {hintFetcher && (
                <div
                    className="mt-2"
                    data-testid={`${testId}-hints`}
                >
                    <div className="small text-muted mb-1">
                        <i className="bi bi-lightbulb me-1" aria-hidden />
                        {hintLabel ?? "Available"}
                    </div>
                    {hints === null && !hintErr && (
                        <div className="small text-muted">Loading…</div>
                    )}
                    {hintErr && (
                        <div
                            className="small text-warning"
                            data-testid={`${testId}-hints-error`}
                        >
                            Couldn't load list ({hintErr}). You can still type
                            names manually above.
                        </div>
                    )}
                    {hints !== null && hints.length === 0 && !hintErr && (
                        <div className="small text-muted">
                            (none registered yet)
                        </div>
                    )}
                    {hints !== null && hints.length > 0 && (
                        <div className="d-flex flex-wrap gap-1">
                            {hints.map((name) => {
                                const already = items.includes(name);
                                return (
                                    <Button
                                        key={name}
                                        variant={
                                            already
                                                ? "outline-secondary"
                                                : "outline-info"
                                        }
                                        size="sm"
                                        className="font-monospace"
                                        style={{ fontSize: 11 }}
                                        onClick={() => appendName(name)}
                                        disabled={already}
                                        title={
                                            already
                                                ? "Already in list"
                                                : "Click to append"
                                        }
                                        data-testid={`${testId}-hint-${name}`}
                                    >
                                        {name}
                                    </Button>
                                );
                            })}
                        </div>
                    )}
                </div>
            )}
        </Form.Group>
    );
}

/// Phase B SetTrust mutator form — dropdown over the canonical
/// trust classes the policy engine matches on. Saves the
/// snake_case alias because that's what the SPA's TypeScript
/// TrustClass union uses (the validator accepts both forms).
const TRUST_CLASSES: Array<{ value: string; label: string }> = [
    { value: "controller", label: "controller (full caps, no spotlighting)" },
    { value: "known_high", label: "known_high (KnownTrusted — full topic access)" },
    { value: "known_limited", label: "known_limited (engages spotlight + planner/executor)" },
    { value: "cold_contact", label: "cold_contact (parks for admission)" },
    { value: "blocked", label: "blocked (drops the turn at the policy gate)" },
];

function SetTrustForm({
    node,
    onChange,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const current = (cfg.trust as string | undefined) ?? "known_limited";
    return (
        <Form.Group className="mb-2">
            <Form.Label className="small text-muted mb-1">Trust class</Form.Label>
            <Form.Select
                size="sm"
                value={current}
                onChange={(e: ChangeEvent<HTMLSelectElement>) =>
                    onChange({ ...node, config: { ...cfg, trust: e.target.value } })
                }
                data-testid="node-panel-set-trust"
            >
                {TRUST_CLASSES.map((t) => (
                    <option key={t.value} value={t.value}>
                        {t.label}
                    </option>
                ))}
            </Form.Select>
            <div className="small text-muted mt-1">
                Overrides the policy engine's resolved sender_trust for
                this turn. Last-writer-wins across matching flows
                (alphabetical by flow name).
            </div>
        </Form.Group>
    );
}

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

const SEND_REPLY_SOURCES = [
    "from_agent",
    "from_node_output",
    "template",
] as const;
type SendReplySource = (typeof SEND_REPLY_SOURCES)[number];

function SendReplyForm({
    node,
    onChange,
}: {
    node: NodeDef;
    onChange: (updated: NodeDef) => void;
}) {
    const cfg = (node.config ?? {}) as Record<string, unknown>;
    const source = ((cfg.source as string | undefined) ?? "from_agent") as SendReplySource;
    const fromNode = (cfg.from_node as string | undefined) ?? "";
    const text = (cfg.text as string | undefined) ?? "";
    const targetOverride = cfg.target_override as unknown;
    const hints = (cfg.hints as Record<string, unknown> | undefined) ?? {};
    const splitPerPart = (hints.split_per_part as boolean | undefined) ?? false;
    const onFailureRaw = hints.on_failure as Record<string, unknown> | undefined;
    const onFailureKind = (onFailureRaw?.kind as string | undefined) ?? "chat_append_home";
    const minChartForm = (hints.min_chart_form as string | undefined) ?? "";

    // target_override is rendered as JSON in a textarea so authors can
    // edit a free-shape OriginRef (the validator on save rejects bad
    // shapes). Empty textarea = field absent = use envelope.origin.
    const [targetDraft, setTargetDraft] = useState(
        targetOverride && targetOverride !== null
            ? JSON.stringify(targetOverride, null, 2)
            : "",
    );
    const [targetErr, setTargetErr] = useState<string | null>(null);

    const update = (next: Record<string, unknown>) =>
        onChange({ ...node, config: { ...cfg, ...next } });

    const updateHints = (next: Record<string, unknown>) =>
        update({ hints: { ...hints, ...next } });

    const commitTarget = () => {
        const trimmed = targetDraft.trim();
        if (trimmed === "") {
            // Clear the field entirely so the server falls back to
            // envelope.origin.
            setTargetErr(null);
            const { target_override: _omit, ...rest } = cfg;
            void _omit;
            onChange({ ...node, config: rest });
            return;
        }
        try {
            const parsed = JSON.parse(trimmed);
            if (
                parsed === null ||
                typeof parsed !== "object" ||
                Array.isArray(parsed) ||
                typeof (parsed as { kind?: unknown }).kind !== "string"
            ) {
                setTargetErr("target_override must be an OriginRef object with a 'kind' field");
                return;
            }
            setTargetErr(null);
            update({ target_override: parsed });
        } catch (e) {
            setTargetErr((e as Error).message);
        }
    };

    return (
        <>
            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Reply source
                </Form.Label>
                <Form.Select
                    size="sm"
                    value={source}
                    onChange={(e: ChangeEvent<HTMLSelectElement>) =>
                        update({ source: e.target.value })
                    }
                    data-testid="node-panel-sendreply-source"
                >
                    <option value="from_agent">
                        from_agent — use upstream AskAgent's exit-tool args
                    </option>
                    <option value="from_node_output">
                        from_node_output — copy a node's output verbatim
                    </option>
                    <option value="template">
                        template — author-supplied templated text
                    </option>
                </Form.Select>
                <div className="small text-muted mt-1">
                    Defaults to <code>from_agent</code> — the common single-
                    AskAgent flow case.
                </div>
            </Form.Group>

            {(source === "from_agent" || source === "from_node_output") && (
                <Form.Group className="mb-2">
                    <Form.Label className="small text-muted mb-1">
                        Upstream node id
                    </Form.Label>
                    <Form.Control
                        type="text"
                        size="sm"
                        value={fromNode}
                        onChange={(e: ChangeEvent<HTMLInputElement>) =>
                            update({ from_node: e.target.value })
                        }
                        placeholder="(auto-detect last AskAgent)"
                        spellCheck={false}
                        className="font-monospace small"
                        data-testid="node-panel-sendreply-from-node"
                    />
                    <div className="small text-muted mt-1">
                        Leave blank to auto-detect the most recent AskAgent
                        output in state.
                    </div>
                </Form.Group>
            )}

            {source === "template" && (
                <Form.Group className="mb-2">
                    <Form.Label className="small text-muted mb-1">
                        Reply text template
                    </Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={3}
                        size="sm"
                        value={text}
                        onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                            update({ text: e.target.value })
                        }
                        placeholder="Acknowledged: {{event.payload.subject}}"
                        spellCheck={false}
                        className="small"
                        data-testid="node-panel-sendreply-text"
                    />
                    <div className="small text-muted mt-1">
                        Supports <code>{`{{event.payload.x}}`}</code> and
                        upstream node refs like{" "}
                        <code>{`{{node_id.field}}`}</code>.
                    </div>
                </Form.Group>
            )}

            <div className="small text-muted mt-2">
                Routes through the ReplyRouter using the trigger event's{" "}
                <code>envelope.origin</code> — web chat replies land via{" "}
                <code>chat_append</code>, channel-plugin replies via the
                plugin's <code>send_reply</code> tool, fire-and-forget
                triggers drop silently.
            </div>

            <hr className="my-3" />
            <div className="small text-muted mb-1">
                <i className="bi bi-sliders me-1" aria-hidden />
                Advanced
            </div>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Target override (OriginRef JSON, optional)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    rows={3}
                    size="sm"
                    value={targetDraft}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        setTargetDraft(e.target.value)
                    }
                    onBlur={commitTarget}
                    placeholder='{"kind":"chat_append","thread_id":"…"}'
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="node-panel-sendreply-target"
                />
                {targetErr && (
                    <div
                        className="small text-danger mt-1"
                        data-testid="node-panel-sendreply-target-error"
                    >
                        {targetErr}
                    </div>
                )}
                <div className="small text-muted mt-1">
                    Defaults to the trigger's <code>envelope.origin</code>.
                    Override to route the reply to a different transport
                    (e.g., always post into the operator Inbox even when
                    triggered by WhatsApp). Empty = use envelope.
                </div>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Check
                    type="checkbox"
                    id={`${node.id}-split-per-part`}
                    label="Split per part (one transport message per ReplyPart)"
                    checked={splitPerPart}
                    onChange={(e) =>
                        updateHints({ split_per_part: e.target.checked })
                    }
                    data-testid="node-panel-sendreply-split-per-part"
                />
                <div className="small text-muted mt-1">
                    WhatsApp/Signal often want one message per attachment
                    for readability; web chat usually wants bundled.
                </div>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    On delivery failure
                </Form.Label>
                <Form.Select
                    size="sm"
                    value={onFailureKind}
                    onChange={(e: ChangeEvent<HTMLSelectElement>) =>
                        updateHints({
                            on_failure: { kind: e.target.value },
                        })
                    }
                    data-testid="node-panel-sendreply-on-failure"
                >
                    <option value="chat_append_home">
                        chat_append_home — append into operator Inbox + alert
                    </option>
                    <option value="alert_only">
                        alert_only — fire alert, drop payload
                    </option>
                    <option value="drop">
                        drop — silently discard
                    </option>
                </Form.Select>
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Minimum chart fidelity (optional)
                </Form.Label>
                <Form.Select
                    size="sm"
                    value={minChartForm}
                    onChange={(e: ChangeEvent<HTMLSelectElement>) => {
                        const v = e.target.value;
                        if (v === "") {
                            // Clear field entirely.
                            const { min_chart_form: _omit, ...rest } = hints;
                            void _omit;
                            update({ hints: rest });
                        } else {
                            updateHints({ min_chart_form: v });
                        }
                    }}
                    data-testid="node-panel-sendreply-min-chart-form"
                >
                    <option value="">— No floor —</option>
                    <option value="inline">inline (web only)</option>
                    <option value="image">image (PNG/JPEG)</option>
                    <option value="url">url</option>
                    <option value="text_ok">text_ok (any)</option>
                </Form.Select>
                <div className="small text-muted mt-1">
                    If the transport can't satisfy this floor, the router
                    refuses to degrade and fires a{" "}
                    <code>ReplyDegradationRefused</code> alert.
                </div>
            </Form.Group>
        </>
    );
}

/**
 * Side panel that opens when the operator clicks the trigger sentinel
 * on the canvas. Audit fix #4+#5: the trigger block was previously
 * only editable from the JSON view. This surfaces both fields
 * (`kind`, `when`) on the canvas and uses the live event registry to
 * populate the kind dropdown so plugin-declared kinds are reachable.
 *
 * The kind dropdown is sourced from
 * `/api/admin/automations/registered-events`. While the request is in
 * flight the dropdown still shows the current value so the form is
 * never empty. Free-text fallback (operator typing a kind that isn't
 * registered yet) is supported via the trailing "Other…" option.
 */
export function TriggerPanel({
    trigger,
    onChange,
    onClose,
}: {
    trigger: TriggerDef;
    onChange: (updated: TriggerDef) => void;
    onClose: () => void;
}) {
    const auth = useAuth();
    const [registry, setRegistry] = useState<RegisteredEventKind[] | null>(null);
    const [registryErr, setRegistryErr] = useState<string | null>(null);
    const [customKind, setCustomKind] = useState<string>("");
    const [usingCustom, setUsingCustom] = useState(false);

    useEffect(() => {
        let cancelled = false;
        listRegisteredEvents(auth.getAccessToken)
            .then((kinds) => {
                if (cancelled) return;
                setRegistry(kinds);
                // If the current trigger.kind isn't in the registry,
                // flip to free-text mode so the operator sees their
                // current value and can keep editing it.
                if (!kinds.some((k) => k.kind === trigger.kind)) {
                    setUsingCustom(true);
                    setCustomKind(trigger.kind);
                }
            })
            .catch((e: Error) => {
                if (cancelled) return;
                setRegistryErr(e.message);
            });
        return () => {
            cancelled = true;
        };
        // Intentionally only run on mount — trigger.kind changes are
        // driven by the operator interacting with this panel, so we
        // don't want the effect re-flipping `usingCustom` mid-edit.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    const selectedFromRegistry = registry?.find((k) => k.kind === trigger.kind);

    return (
        <div
            className="execlaw-automation-trigger-panel border rounded shadow-sm"
            style={{
                position: "absolute",
                top: 12,
                right: 12,
                width: 380,
                maxHeight: "calc(100% - 24px)",
                overflowY: "auto",
                zIndex: 10,
                padding: 12,
                background: "#161b22",
                borderColor: "#30363d",
                color: "#e6edf3",
                boxShadow: "0 4px 12px rgba(0, 0, 0, 0.45)",
            }}
            data-testid="trigger-panel"
            onKeyDown={(e) => e.stopPropagation()}
        >
            <div className="d-flex justify-content-between align-items-start mb-2">
                <div>
                    <div className="text-muted small">Trigger</div>
                    <div className="h6 mb-0">
                        <i className="bi bi-lightning-fill me-1" aria-hidden />
                        Event entrypoint
                    </div>
                </div>
                <Button
                    variant="link"
                    size="sm"
                    onClick={onClose}
                    aria-label="Close panel"
                    data-testid="trigger-panel-close"
                >
                    <i className="bi bi-x-lg" aria-hidden />
                </Button>
            </div>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">Event kind</Form.Label>
                {registry === null && !registryErr && (
                    <div className="small text-muted mb-1">
                        Loading registered events…
                    </div>
                )}
                {registryErr && (
                    <div
                        className="small text-warning mb-1"
                        data-testid="trigger-panel-registry-error"
                    >
                        Failed to load registry ({registryErr}). Falling
                        back to free-text entry.
                    </div>
                )}
                {!usingCustom && registry !== null && (
                    <Form.Select
                        size="sm"
                        value={trigger.kind}
                        onChange={(e: ChangeEvent<HTMLSelectElement>) => {
                            const next = e.target.value;
                            if (next === "__custom__") {
                                setUsingCustom(true);
                                setCustomKind(trigger.kind);
                                return;
                            }
                            onChange({ ...trigger, kind: next });
                        }}
                        data-testid="trigger-panel-kind"
                    >
                        {/* If the current kind isn't registered (shouldn't
                            happen with the useEffect above, but defensive)
                            show it at top so the form isn't blank. */}
                        {!registry.some((k) => k.kind === trigger.kind) && (
                            <option value={trigger.kind}>
                                {trigger.kind} (not registered)
                            </option>
                        )}
                        {registry.map((k) => (
                            <option key={k.kind} value={k.kind}>
                                {k.kind} — {k.source}
                            </option>
                        ))}
                        <option value="__custom__">
                            Other (type a custom kind)…
                        </option>
                    </Form.Select>
                )}
                {(usingCustom || registryErr) && (
                    <div className="d-flex gap-2 align-items-start">
                        <Form.Control
                            type="text"
                            size="sm"
                            value={customKind}
                            onChange={(e: ChangeEvent<HTMLInputElement>) =>
                                setCustomKind(e.target.value)
                            }
                            onBlur={() => {
                                const trimmed = customKind.trim();
                                if (trimmed && trimmed !== trigger.kind) {
                                    onChange({ ...trigger, kind: trimmed });
                                }
                            }}
                            placeholder="e.g. my_plugin.thing.happened"
                            spellCheck={false}
                            className="font-monospace small"
                            data-testid="trigger-panel-kind-custom"
                        />
                        {registry !== null && !registryErr && (
                            <Button
                                variant="outline-secondary"
                                size="sm"
                                onClick={() => setUsingCustom(false)}
                                title="Back to registered kinds"
                            >
                                <i className="bi bi-arrow-left" aria-hidden />
                            </Button>
                        )}
                    </div>
                )}
                {selectedFromRegistry && (
                    <div
                        className="small text-muted mt-1"
                        data-testid="trigger-panel-kind-description"
                    >
                        {selectedFromRegistry.description || "(no description)"}
                        {" "}
                        <span style={{ color: "#7d8590" }}>
                            · expects_reply ={" "}
                            <code>{String(selectedFromRegistry.expects_reply)}</code>
                        </span>
                    </div>
                )}
            </Form.Group>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    Trigger filter (Rhai bool, optional)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    rows={3}
                    value={trigger.when ?? ""}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        onChange({
                            ...trigger,
                            when: e.target.value === "" ? null : e.target.value,
                        })
                    }
                    placeholder='event.envelope.identity.trust == "controller"'
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="trigger-panel-when"
                />
                <div className="small text-muted mt-1">
                    Evaluated for every event of this kind. Falsy → the
                    flow doesn't run. The scope exposes{" "}
                    <code>event.kind</code>, <code>event.source</code>,{" "}
                    <code>event.payload</code>, <code>event.envelope</code>,{" "}
                    <code>event.internal</code>.
                </div>
            </Form.Group>

            <TriggerPanelPluginSuggestions
                trigger={trigger}
                onPick={(rendered) =>
                    onChange({
                        ...trigger,
                        when: rendered,
                    })
                }
            />

            <div className="small text-muted mt-2">
                The trigger and the End sentinel are structural — every
                flow has exactly one of each. Drag operator nodes from
                the palette and wire them between trigger and End.
            </div>
        </div>
    );
}

/**
 * Slice G (2026-05-26) — plugin-contributed suggestions for the
 * trigger.when picker. Filters by the active trigger.kind. The
 * `when_active` heuristic is dropped here because the trigger has
 * no "upstream context" — the operator is AUTHORING the narrow, so
 * every suggestion for the current kind is relevant. Each pick
 * replaces the entire `when` field (operators can compose multiple
 * narrows by hand if they want; we don't AND them silently).
 */
function TriggerPanelPluginSuggestions({
    trigger,
    onPick,
}: {
    trigger: TriggerDef;
    onPick: (rendered: string) => void;
}) {
    const pluginSuggestions = useBranchSuggestions();
    const filtered = useMemo<BranchSuggestion[]>(() => {
        // For trigger.when the activeContext is the existing
        // trigger.when text — lets operators iteratively narrow
        // (slack → then by channel name).
        return filterBranchSuggestions(
            pluginSuggestions,
            trigger.kind,
            trigger.when ?? "",
        );
    }, [pluginSuggestions, trigger.kind, trigger.when]);
    if (filtered.length === 0) return null;
    return (
        <div
            className="mb-2"
            data-testid="trigger-panel-plugin-suggestions"
        >
            <div className="small text-muted mb-1">
                <i className="bi bi-puzzle me-1" aria-hidden />
                Suggested filters from installed plugins
            </div>
            <div className="d-flex flex-column gap-1">
                {filtered.map((s, i) => {
                    const rendered = renderBranchSuggestion(s);
                    return (
                        <Button
                            key={`${s.source_plugin_id}-${i}`}
                            variant="outline-secondary"
                            size="sm"
                            className="text-start"
                            style={{ fontSize: 12 }}
                            onClick={() => onPick(rendered)}
                            data-testid={`trigger-panel-plugin-suggestion-${i}`}
                            title={s.description}
                        >
                            <div>
                                <span className="me-2">{s.display_name}</span>
                                <span
                                    className="badge text-bg-secondary"
                                    style={{ fontSize: 10 }}
                                >
                                    {s.source_plugin_id}
                                </span>
                            </div>
                            <div
                                className="font-monospace text-muted"
                                style={{ fontSize: 11 }}
                            >
                                {rendered}
                            </div>
                        </Button>
                    );
                })}
            </div>
        </div>
    );
}

/**
 * Side panel for editing an edge's `when` clause (audit fix #3).
 * Edges previously had no canvas-side editor — operators had to drop
 * into the JSON view to gate a branch on the upstream AskAgent's
 * exit-tool name. This surfaces that field directly.
 *
 * The `when` clause is a Rhai bool expression evaluated against the
 * same scope as trigger filters (`event.*`) plus the per-node state
 * keys (e.g. `agent1.outcome` when `agent1` is an upstream AskAgent
 * node). Empty/whitespace text persists as `null` — the runtime
 * treats null as "always taken".
 */
export function EdgePanel({
    edgeId,
    edge,
    definition,
    onWhenChange,
    onClose,
}: {
    edgeId: string;
    edge: EdgeDef;
    /** Whole def — needed so we can look up the upstream node's
     *  exit_tools and suggest click-to-paste `when` snippets. */
    definition: AutomationDef;
    /** Replace the edge's `when` clause. Pass `null` to clear it. */
    onWhenChange: (edgeId: string, whenExpr: string | null) => void;
    onClose: () => void;
}) {
    // Draft state — local mutation flushes back to the canonical def
    // on blur so each keystroke doesn't churn the AutomationDef object
    // (which would re-render the whole canvas + re-seed ReactFlow).
    const [draft, setDraft] = useState<string>(edge.when ?? "");
    const commit = () => {
        const trimmed = draft.trim();
        const next = trimmed === "" ? null : draft;
        if (next !== edge.when) {
            onWhenChange(edgeId, next);
        }
    };

    /** Suggestions for the `when` clause based on the upstream node's
     *  kind. AskAgent exits via a tool call whose name is recorded as
     *  `{node_id}.tool` in state — so the canonical predicate is
     *  `<node_id>.tool == "<exit_tool>"`. Audit fix #10. */
    const suggestions = useMemo<string[]>(() => {
        if (edge.from === "trigger") {
            return [
                'event.envelope.identity.trust == "controller"',
                'event.envelope.origin.kind == "web_socket_session"',
                "event.internal == false",
            ];
        }
        const upstream = definition.nodes.find((n) => n.id === edge.from);
        if (!upstream) return [];
        if (upstream.kind === "AskAgent") {
            const cfg = (upstream.config ?? {}) as Record<string, unknown>;
            const exitTools = (cfg.exit_tools as ExitToolDef[] | undefined) ?? [];
            return exitTools.map(
                (t) => `${edge.from}.tool == "${t.name}"`,
            );
        }
        if (upstream.kind === "Filter" || upstream.kind === "Transform") {
            // For Transform, the node output is whatever the Rhai
            // expression returned. We don't know the shape — suggest
            // a generic pattern.
            return [`${edge.from}.field == "value"`];
        }
        if (upstream.kind === "CallPlugin") {
            return [
                `${edge.from}.ok == true`,
                `${edge.from}.status == 200`,
            ];
        }
        return [];
    }, [definition, edge.from]);

    // Slice G (2026-05-26) — plugin-contributed branch suggestions
    // filtered to the flow's active trigger kind + the channel-narrow
    // context (trigger.when). The when_active gate is a string-
    // contains heuristic so a slack-narrowed flow only sees slack
    // suggestions, etc.
    const pluginSuggestions = useBranchSuggestions();
    const filteredPluginSuggestions = useMemo<BranchSuggestion[]>(() => {
        return filterBranchSuggestions(
            pluginSuggestions,
            definition.trigger.kind,
            // For edges, the context that decides "is this slack-
            // narrowed?" is the flow's trigger.when. An edge inside
            // a slack-narrowed flow inherits the channel constraint.
            definition.trigger.when ?? "",
        );
    }, [pluginSuggestions, definition.trigger.kind, definition.trigger.when]);
    return (
        <div
            className="execlaw-automation-edge-panel border rounded shadow-sm"
            style={{
                position: "absolute",
                top: 12,
                right: 12,
                width: 380,
                maxHeight: "calc(100% - 24px)",
                overflowY: "auto",
                zIndex: 10,
                padding: 12,
                background: "#161b22",
                borderColor: "#30363d",
                color: "#e6edf3",
                boxShadow: "0 4px 12px rgba(0, 0, 0, 0.45)",
            }}
            data-testid="edge-panel"
            onKeyDown={(e) => e.stopPropagation()}
        >
            <div className="d-flex justify-content-between align-items-start mb-2">
                <div>
                    <div className="text-muted small">Edge</div>
                    <div
                        className="h6 mb-0 font-monospace"
                        data-testid="edge-panel-route"
                    >
                        {edge.from} → {edge.to}
                    </div>
                </div>
                <Button
                    variant="link"
                    size="sm"
                    onClick={onClose}
                    aria-label="Close panel"
                    data-testid="edge-panel-close"
                >
                    <i className="bi bi-x-lg" aria-hidden />
                </Button>
            </div>

            <Form.Group className="mb-2">
                <Form.Label className="small text-muted mb-1">
                    When (Rhai bool, optional)
                </Form.Label>
                <Form.Control
                    as="textarea"
                    rows={3}
                    value={draft}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) =>
                        setDraft(e.target.value)
                    }
                    onBlur={commit}
                    onKeyDown={(e) => {
                        // Ctrl/Cmd+Enter commits without leaving the
                        // panel — useful for quick iteration on a
                        // branch condition.
                        if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
                            e.preventDefault();
                            commit();
                        }
                    }}
                    placeholder={
                        edge.from === "trigger"
                            ? 'event.payload.zone == "driveway"'
                            : `${edge.from}.outcome == "notify"`
                    }
                    spellCheck={false}
                    className="font-monospace small"
                    data-testid="edge-panel-when"
                />
                <div className="small text-muted mt-1">
                    Falsy or expression error → this edge isn't taken.
                    Leave blank for an unconditional edge. Scope
                    exposes <code>event.*</code> plus each upstream
                    node's output as <code>{`{node_id}.field`}</code>.
                </div>
            </Form.Group>

            {suggestions.length > 0 && (
                <div className="mb-2" data-testid="edge-panel-suggestions">
                    <div className="small text-muted mb-1">
                        <i className="bi bi-lightbulb me-1" aria-hidden />
                        Suggestions {edge.from !== "trigger" && `from ${edge.from}`}
                    </div>
                    <div className="d-flex flex-wrap gap-1">
                        {suggestions.map((s, i) => (
                            <Button
                                key={i}
                                variant="outline-secondary"
                                size="sm"
                                className="font-monospace"
                                style={{ fontSize: 11 }}
                                onClick={() => {
                                    setDraft(s);
                                    onWhenChange(edgeId, s);
                                }}
                                data-testid={`edge-panel-suggestion-${i}`}
                            >
                                {s}
                            </Button>
                        ))}
                    </div>
                </div>
            )}

            {filteredPluginSuggestions.length > 0 && (
                <div
                    className="mb-2"
                    data-testid="edge-panel-plugin-suggestions"
                >
                    <div className="small text-muted mb-1">
                        <i className="bi bi-puzzle me-1" aria-hidden />
                        Suggested splits from installed plugins
                    </div>
                    <div className="d-flex flex-column gap-1">
                        {filteredPluginSuggestions.map((s, i) => {
                            const rendered = renderBranchSuggestion(s);
                            return (
                                <Button
                                    key={`${s.source_plugin_id}-${i}`}
                                    variant="outline-secondary"
                                    size="sm"
                                    className="text-start"
                                    style={{ fontSize: 12 }}
                                    onClick={() => {
                                        setDraft(rendered);
                                        onWhenChange(edgeId, rendered);
                                    }}
                                    data-testid={`edge-panel-plugin-suggestion-${i}`}
                                    title={s.description}
                                >
                                    <div>
                                        <span className="me-2">
                                            {s.display_name}
                                        </span>
                                        <span
                                            className="badge text-bg-secondary"
                                            style={{ fontSize: 10 }}
                                        >
                                            {s.source_plugin_id}
                                        </span>
                                    </div>
                                    <div
                                        className="font-monospace text-muted"
                                        style={{ fontSize: 11 }}
                                    >
                                        {rendered}
                                    </div>
                                </Button>
                            );
                        })}
                    </div>
                </div>
            )}

            <Button
                variant="outline-secondary"
                size="sm"
                onClick={() => {
                    setDraft("");
                    onWhenChange(edgeId, null);
                }}
                disabled={!edge.when && draft === ""}
                data-testid="edge-panel-clear"
            >
                <i className="bi bi-eraser me-1" aria-hidden />
                Clear condition
            </Button>

            <div className="small text-muted mt-3">
                Multiple outgoing edges from a Branch node each get
                their own <code>when</code>; the first matching edge
                wins. From a non-Branch node, an unconditional edge is
                taken always.
            </div>
        </div>
    );
}
