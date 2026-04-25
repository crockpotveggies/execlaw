// Settings → MCP servers (Phase 8c/d).
//
// Lists every configured MCP server with status indicator, lets the
// Controller add / edit / disable / delete servers. After every
// mutation the server-side reconciles its tokio actor set, so a
// new server typically transitions idle → connected within a second
// or two; the operator can hit Refresh to see the updated status.
//
// Discovered tools live on the Settings → Tools page (Phase 8a) —
// this page is just the connection bookkeeping.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    createMcpServer,
    deleteMcpServer,
    listMcpServers,
    updateMcpServer,
    type McpServerView,
    type McpServerWriteRequest,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

const TRUST_CLASSES: ReadonlyArray<string> = [
    "Controller",
    "Delegated",
    "KnownTrusted",
    "KnownLimited",
    "UnknownPending",
    "Blocked",
];

const STATUS_BADGE: Record<McpServerView["status"], string> = {
    idle: "is-limited",
    connected: "is-controller",
    disconnected: "is-known",
    error: "is-limited",
};

interface FormState {
    id: string;
    display_name: string;
    command: string;
    args: string; // newline-separated
    env: string; // KEY=VALUE per line
    cwd: string;
    enabled: boolean;
    default_allowed_classes: string[];
}

const EMPTY_FORM: FormState = {
    id: "",
    display_name: "",
    command: "",
    args: "",
    env: "",
    cwd: "",
    enabled: true,
    default_allowed_classes: ["Controller"],
};

function fromRow(s: McpServerView): FormState {
    return {
        id: s.id,
        display_name: s.display_name,
        command: s.command ?? "",
        args: s.args.join("\n"),
        env: Object.entries(s.env)
            .map(([k, v]) => `${k}=${v}`)
            .join("\n"),
        cwd: s.cwd ?? "",
        enabled: s.enabled,
        default_allowed_classes: s.default_allowed_classes,
    };
}

function toRequest(f: FormState): McpServerWriteRequest {
    const env: Record<string, string> = {};
    for (const line of f.env.split("\n")) {
        const trimmed = line.trim();
        if (trimmed.length === 0) continue;
        const eq = trimmed.indexOf("=");
        if (eq <= 0) continue;
        env[trimmed.slice(0, eq)] = trimmed.slice(eq + 1);
    }
    const args = f.args
        .split("\n")
        .map((s) => s.trim())
        .filter((s) => s.length > 0);
    return {
        id: f.id.trim(),
        display_name: f.display_name.trim(),
        transport: "stdio",
        command: f.command.trim().length > 0 ? f.command.trim() : null,
        args,
        env,
        cwd: f.cwd.trim().length > 0 ? f.cwd.trim() : null,
        enabled: f.enabled,
        default_allowed_classes: f.default_allowed_classes,
    };
}

export function McpServersPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [servers, setServers] = useState<McpServerView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [editingId, setEditingId] = useState<string | "__new__" | null>(null);
    const [form, setForm] = useState<FormState>(EMPTY_FORM);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await listMcpServers(getToken);
            setServers(r.servers);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    const onSave = useCallback(async () => {
        setBusy(true);
        setError(null);
        try {
            const body = toRequest(form);
            if (editingId === "__new__") {
                await createMcpServer(body, getToken);
            } else if (editingId) {
                await updateMcpServer(editingId, body, getToken);
            }
            setEditingId(null);
            setForm(EMPTY_FORM);
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [editingId, form, getToken, refresh]);

    const onDelete = useCallback(
        async (s: McpServerView) => {
            if (
                !confirm(
                    `Delete MCP server "${s.display_name}"? Its tools will be marked removed and the dispatch gate will deny further calls.`,
                )
            )
                return;
            try {
                await deleteMcpServer(s.id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken, refresh],
    );

    const toggleClass = (cls: string) => {
        setForm((f) => ({
            ...f,
            default_allowed_classes: f.default_allowed_classes.includes(cls)
                ? f.default_allowed_classes.filter((c) => c !== cls)
                : [...f.default_allowed_classes, cls],
        }));
    };

    return (
        <div data-testid="settings-mcp">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">MCP servers</h3>
                <Button
                    size="sm"
                    variant="outline-secondary"
                    onClick={() => void refresh()}
                    className="me-2"
                    data-testid="mcp-refresh"
                >
                    <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                    Refresh
                </Button>
                {canMutate && editingId === null && (
                    <Button
                        size="sm"
                        variant="primary"
                        onClick={() => {
                            setEditingId("__new__");
                            setForm(EMPTY_FORM);
                            setError(null);
                        }}
                        data-testid="mcp-add"
                    >
                        <i className="bi bi-plus-lg me-1" aria-hidden />
                        Add server
                    </Button>
                )}
            </div>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can manage MCP servers.
                </div>
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {editingId !== null && (
                <div className="execlaw-card mb-3" data-testid="mcp-form">
                    <div className="execlaw-card__title mb-2">
                        {editingId === "__new__" ? "Add MCP server" : `Edit ${editingId}`}
                    </div>
                    <div className="row g-2 mb-2">
                        <Form.Group className="col-sm-4">
                            <Form.Label className="execlaw-muted small mb-1">
                                Id (slug)
                            </Form.Label>
                            <Form.Control
                                value={form.id}
                                onChange={(e) =>
                                    setForm({ ...form, id: e.target.value })
                                }
                                disabled={editingId !== "__new__"}
                                placeholder="github"
                                data-testid="mcp-form-id"
                            />
                        </Form.Group>
                        <Form.Group className="col-sm-8">
                            <Form.Label className="execlaw-muted small mb-1">
                                Display name
                            </Form.Label>
                            <Form.Control
                                value={form.display_name}
                                onChange={(e) =>
                                    setForm({ ...form, display_name: e.target.value })
                                }
                                placeholder="GitHub MCP"
                                data-testid="mcp-form-name"
                            />
                        </Form.Group>
                    </div>
                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Command (stdio)
                        </Form.Label>
                        <Form.Control
                            value={form.command}
                            onChange={(e) =>
                                setForm({ ...form, command: e.target.value })
                            }
                            placeholder="/usr/local/bin/github-mcp"
                            data-testid="mcp-form-command"
                        />
                    </Form.Group>
                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Args (one per line)
                        </Form.Label>
                        <Form.Control
                            as="textarea"
                            rows={3}
                            value={form.args}
                            onChange={(e) =>
                                setForm({ ...form, args: e.target.value })
                            }
                            placeholder={"--repo\nowner/repo"}
                            data-testid="mcp-form-args"
                        />
                    </Form.Group>
                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Environment (KEY=VALUE per line)
                        </Form.Label>
                        <Form.Control
                            as="textarea"
                            rows={3}
                            value={form.env}
                            onChange={(e) =>
                                setForm({ ...form, env: e.target.value })
                            }
                            placeholder="GITHUB_TOKEN=xxxxxxxx"
                            spellCheck={false}
                            data-testid="mcp-form-env"
                        />
                    </Form.Group>
                    <div className="mb-2">
                        <span className="execlaw-muted small me-2">
                            Default allowed classes:
                        </span>
                        {TRUST_CLASSES.map((cls) => (
                            <Form.Check
                                inline
                                key={cls}
                                type="checkbox"
                                id={`mcp-form-class-${cls}`}
                                label={cls}
                                checked={form.default_allowed_classes.includes(cls)}
                                onChange={() => toggleClass(cls)}
                                data-testid="mcp-form-class"
                                data-class={cls}
                            />
                        ))}
                    </div>
                    <Form.Check
                        type="switch"
                        id="mcp-form-enabled"
                        label="Enabled"
                        checked={form.enabled}
                        onChange={(e) =>
                            setForm({ ...form, enabled: e.target.checked })
                        }
                        className="mb-3"
                        data-testid="mcp-form-enabled"
                    />
                    <div className="d-flex gap-2">
                        <Button
                            variant="primary"
                            disabled={busy || form.id.trim().length === 0}
                            onClick={() => void onSave()}
                            data-testid="mcp-form-save"
                        >
                            Save
                        </Button>
                        <Button
                            variant="outline-secondary"
                            onClick={() => {
                                setEditingId(null);
                                setForm(EMPTY_FORM);
                            }}
                        >
                            Cancel
                        </Button>
                    </div>
                </div>
            )}

            {servers === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : servers.length === 0 ? (
                <div className="execlaw-muted small">
                    No MCP servers configured. Click <strong>Add server</strong> to
                    point execlaw at one — its tools will appear in the
                    Tools page once the connection succeeds.
                </div>
            ) : (
                servers.map((s) => (
                    <div
                        className="execlaw-card"
                        key={s.id}
                        data-testid="mcp-row"
                        data-mcp-id={s.id}
                    >
                        <div className="d-flex align-items-center gap-2 mb-1">
                            <span className="execlaw-card__title flex-grow-1">
                                {s.display_name}
                                <span className="execlaw-muted ms-2">
                                    <code>{s.id}</code>
                                </span>
                                <span
                                    className={`execlaw-trust-badge ms-2 ${STATUS_BADGE[s.status]}`}
                                >
                                    {s.status}
                                </span>
                                {!s.enabled && (
                                    <span className="execlaw-trust-badge ms-2 is-limited">
                                        disabled
                                    </span>
                                )}
                            </span>
                            {canMutate && (
                                <>
                                    <Button
                                        size="sm"
                                        variant="outline-primary"
                                        onClick={() => {
                                            setEditingId(s.id);
                                            setForm(fromRow(s));
                                            setError(null);
                                        }}
                                        data-testid="mcp-edit"
                                    >
                                        Edit
                                    </Button>
                                    <Button
                                        size="sm"
                                        variant="outline-danger"
                                        onClick={() => void onDelete(s)}
                                        data-testid="mcp-delete"
                                    >
                                        Delete
                                    </Button>
                                </>
                            )}
                        </div>
                        <div className="execlaw-muted small mb-2">
                            transport <strong>{s.transport}</strong>
                            {s.command && (
                                <>
                                    {" · "}
                                    <code>{s.command}</code>
                                </>
                            )}
                            {s.args.length > 0 && (
                                <>
                                    {" "}
                                    <code>{s.args.join(" ")}</code>
                                </>
                            )}
                        </div>
                        {s.last_error && (
                            <div className="execlaw-error-banner small mb-2" role="alert">
                                {s.last_error}
                            </div>
                        )}
                    </div>
                ))
            )}
        </div>
    );
}
