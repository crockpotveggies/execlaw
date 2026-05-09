// Settings → Tools (Phase 8a per-tool trust-class allowlist).
//
// Lists every tool the runner might dispatch — builtins, plugin
// tools, and (Phase 8b+) MCP-server tools — with a per-row toggle
// for `enabled` and a multi-select for the trust-class allowlist.
// Mutations go through PATCH /api/admin/tools/{tool_name} (Controller-
// only on the server side; the SPA hides the controls when the
// caller isn't a Controller).

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    listTools,
    updateToolPolicy,
    type ToolView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

const TRUST_CLASSES: ReadonlyArray<string> = [
    "Controller",
    "Delegated",
    "KnownTrusted",
    "KnownLimited",
    "UnknownPending",
    "Blocked",
];

const SOURCE_BADGE: Record<ToolView["source"], string> = {
    builtin: "is-known",
    plugin: "is-controller",
    mcp: "is-limited",
};

export function ToolsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [tools, setTools] = useState<ToolView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyTool, setBusyTool] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listTools(getToken);
            setTools(r.tools);
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

    const onToggleEnabled = useCallback(
        async (tool: ToolView) => {
            setBusyTool(tool.tool_name);
            try {
                await updateToolPolicy(
                    tool.tool_name,
                    {
                        enabled: !tool.enabled,
                        allowed_classes: tool.allowed_classes,
                    },
                    getToken,
                );
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyTool(null);
            }
        },
        [getToken, refresh],
    );

    const onToggleClass = useCallback(
        async (tool: ToolView, cls: string) => {
            const next = tool.allowed_classes.includes(cls)
                ? tool.allowed_classes.filter((c) => c !== cls)
                : [...tool.allowed_classes, cls];
            setBusyTool(tool.tool_name);
            try {
                await updateToolPolicy(
                    tool.tool_name,
                    { enabled: tool.enabled, allowed_classes: next },
                    getToken,
                );
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyTool(null);
            }
        },
        [getToken, refresh],
    );

    return (
        <div data-testid="settings-tools">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Tools</h3>
                <Button
                    size="sm"
                    variant="outline-secondary"
                    onClick={() => void refresh()}
                    data-testid="tools-refresh"
                >
                    <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                    Refresh
                </Button>
            </div>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can change tool access policy.
                </div>
            )}

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            {tools === null ? (
                <div className="execlaw-muted small">Loading tools…</div>
            ) : tools.length === 0 ? (
                <div className="execlaw-muted small">
                    No tools registered yet. Install a plugin or wire up
                    an MCP server to populate the list.
                </div>
            ) : (
                tools.map((t) => (
                    <div
                        className="execlaw-card"
                        key={t.tool_name}
                        data-testid="tool-row"
                        data-tool-name={t.tool_name}
                    >
                        <div className="d-flex align-items-center gap-2 mb-1">
                            <span className="execlaw-card__title flex-grow-1">
                                <code>{t.tool_name}</code>
                                <span
                                    className={`execlaw-trust-badge ms-2 ${SOURCE_BADGE[t.source]}`}
                                >
                                    {t.source}
                                </span>
                                {t.removed_at !== null && (
                                    <span className="execlaw-trust-badge ms-2 is-limited">
                                        removed
                                    </span>
                                )}
                                {!t.enabled && (
                                    <span className="execlaw-trust-badge ms-2 is-limited">
                                        disabled
                                    </span>
                                )}
                            </span>
                            {canMutate && (
                                <Form.Check
                                    type="switch"
                                    id={`enabled-${t.tool_name}`}
                                    label="enabled"
                                    checked={t.enabled}
                                    disabled={busyTool === t.tool_name}
                                    onChange={() => void onToggleEnabled(t)}
                                    data-testid="tool-enabled-toggle"
                                />
                            )}
                        </div>
                        {t.description && (
                            <div className="execlaw-muted small mb-2">
                                {t.description}
                            </div>
                        )}
                        <div className="d-flex flex-wrap gap-2 align-items-center">
                            <span className="execlaw-muted small me-2">
                                Allowed:
                            </span>
                            {TRUST_CLASSES.map((cls) => {
                                const checked = t.allowed_classes.includes(cls);
                                return (
                                    <Form.Check
                                        key={cls}
                                        type="checkbox"
                                        inline
                                        id={`${t.tool_name}-${cls}`}
                                        label={cls}
                                        checked={checked}
                                        disabled={
                                            !canMutate || busyTool === t.tool_name
                                        }
                                        onChange={() => void onToggleClass(t, cls)}
                                        data-testid="tool-class-checkbox"
                                        data-class={cls}
                                    />
                                );
                            })}
                        </div>
                    </div>
                ))
            )}
        </div>
    );
}
