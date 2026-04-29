// Settings → Skills.
//
// "What can my agent actually do" view, grouped by source. Distinct
// from:
//   * Plugins page — install / enable / disable lifecycle.
//   * Tools page   — flat per-tool registry with trust-class policy.
//
// This page is a derived projection: pulls the tools registry (which
// is the canonical source of every callable capability — built-in,
// plugin-contributed, or MCP-reflected) and groups it by source so
// the operator sees "MCP server `slack` adds 7 tools; plugin
// `google-calendar` adds 4; 12 tools are built-in" at a glance.
//
// Drilling into a group is deferred — link out to the Tools page for
// per-tool policy edits.

import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import {
    listTools,
    type ToolSource,
    type ToolView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

interface SkillGroup {
    source: ToolSource;
    /** For plugin/MCP: the plugin id or server name. Empty for built-ins. */
    sourceId: string;
    /** Display label — collapses the source + id pair into one string. */
    label: string;
    enabled: number;
    disabled: number;
    tools: ToolView[];
}

function groupTools(tools: ToolView[]): SkillGroup[] {
    const map = new Map<string, SkillGroup>();
    for (const t of tools) {
        if (t.removed_at !== null) continue; // hide tombstoned tools
        const sourceId = t.source_id ?? "";
        const key = `${t.source}:${sourceId}`;
        const label = sourceLabel(t.source, sourceId);
        const existing = map.get(key);
        if (existing) {
            existing.tools.push(t);
            if (t.enabled) existing.enabled += 1;
            else existing.disabled += 1;
        } else {
            map.set(key, {
                source: t.source,
                sourceId,
                label,
                enabled: t.enabled ? 1 : 0,
                disabled: t.enabled ? 0 : 1,
                tools: [t],
            });
        }
    }
    return Array.from(map.values()).sort((a, b) => {
        // Built-ins first, then plugin, then MCP — matches the order
        // an operator thinks about capabilities ("ours, ours, theirs").
        const rank = { builtin: 0, plugin: 1, mcp: 2 } as const;
        const r = rank[a.source] - rank[b.source];
        if (r !== 0) return r;
        return a.label.localeCompare(b.label);
    });
}

function sourceLabel(source: ToolSource, id: string): string {
    if (source === "builtin") return "Built-in";
    if (source === "plugin") return id ? `Plugin · ${id}` : "Plugin";
    if (source === "mcp") return id ? `MCP · ${id}` : "MCP server";
    return source;
}

function sourceIcon(source: ToolSource): string {
    switch (source) {
        case "builtin":
            return "bi-tools";
        case "plugin":
            return "bi-puzzle";
        case "mcp":
            return "bi-broadcast";
    }
}

export function SkillsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [tools, setTools] = useState<ToolView[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await listTools(getToken);
                if (!cancelled) {
                    setTools(r.tools);
                    setError(null);
                }
            } catch (e) {
                if (!cancelled)
                    setError(e instanceof Error ? e.message : String(e));
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    const groups = useMemo(
        () => (tools === null ? null : groupTools(tools)),
        [tools],
    );

    const totalEnabled = useMemo(() => {
        if (!groups) return 0;
        return groups.reduce((acc, g) => acc + g.enabled, 0);
    }, [groups]);

    return (
        <div data-testid="settings-skills">
            <div className="d-flex align-items-baseline gap-2 mb-2">
                <h3 className="h6 mb-0 flex-grow-1">Skills</h3>
                {groups !== null && (
                    <span className="execlaw-muted small">
                        {totalEnabled} enabled · {groups.length} sources
                    </span>
                )}
            </div>
            <p className="execlaw-muted small mb-3">
                Capabilities your agent can call, grouped by source.
                Built-in tools are always present; plugin and MCP groups
                appear as you install or connect them. Edit per-tool
                policy in <Link to="/settings/tools">Tools</Link>; manage
                installs in <Link to="/settings/plugins">Plugins</Link> or{" "}
                <Link to="/settings/mcp">MCP</Link>.
            </p>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            {groups === null ? (
                <div className="execlaw-muted small">Loading skills…</div>
            ) : groups.length === 0 ? (
                <div
                    className="execlaw-muted small"
                    data-testid="skills-empty"
                >
                    No skills yet. Install a plugin or connect an MCP server
                    to give the agent something to do.
                </div>
            ) : (
                groups.map((g) => (
                    <div
                        className="execlaw-card"
                        key={`${g.source}:${g.sourceId}`}
                        data-testid="skill-group"
                    >
                        <div className="d-flex align-items-center gap-2 mb-2">
                            <i
                                className={`bi ${sourceIcon(g.source)} execlaw-muted`}
                                aria-hidden
                            />
                            <span className="execlaw-card__title flex-grow-1">
                                {g.label}
                            </span>
                            <span className="execlaw-muted small">
                                {g.enabled + g.disabled} tool
                                {g.enabled + g.disabled === 1 ? "" : "s"}
                                {g.disabled > 0 && ` · ${g.disabled} disabled`}
                            </span>
                        </div>
                        <ul className="list-unstyled mb-0 small">
                            {g.tools
                                .slice()
                                .sort((a, b) =>
                                    a.tool_name.localeCompare(b.tool_name),
                                )
                                .map((t) => (
                                    <li
                                        key={t.tool_name}
                                        className="d-flex align-items-baseline gap-2 mb-1"
                                        data-testid="skill-tool"
                                    >
                                        <code>{t.tool_name}</code>
                                        {!t.enabled && (
                                            <span className="execlaw-muted">
                                                (disabled)
                                            </span>
                                        )}
                                        {t.description && (
                                            <span className="execlaw-muted flex-grow-1">
                                                {t.description}
                                            </span>
                                        )}
                                    </li>
                                ))}
                        </ul>
                    </div>
                ))
            )}
        </div>
    );
}
