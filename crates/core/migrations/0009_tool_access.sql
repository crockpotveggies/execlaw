-- 0009: Per-tool access policy (Phase 8a — foundation for MCP integration).
--
-- Generalises what used to live implicitly in the trust-class →
-- capability-set mapping: now every tool the runner might invoke has
-- one row in this table that decides:
--
--   * Whether it's enabled at all (operator override of the plugin /
--     MCP server's wishes).
--   * Which trust classes can call it (allowlist; default-deny when
--     the list is empty for safety).
--
-- The schema covers all three sources — built-in runner tools (memory,
-- thread), plugin-supplied tools (subprocess), and MCP-server tools
-- (Phase 8b+) — so the Settings → Tools page lists them in one place
-- and the dispatch gate is ONE check regardless of provenance.
--
-- `tool_name` is the canonical name the runner knows the tool by:
--   * Built-ins: bare names like `set_thread_name`.
--   * Plugin tools: bare names today (Phase-2 didn't namespace them).
--   * MCP tools: prefixed `mcp:<server_id>:<tool_name>` so two
--     servers offering the same upstream tool can coexist.
--
-- `allowed_classes` is a JSON array of `TrustClass` strings — the
-- exact same vocabulary the policy crate already uses (Controller,
-- KnownTrusted, Contact, UnknownPending, Blocked). An empty array
-- means "no class can use this tool"; a NULL is invalid (NOT NULL).
--
-- `input_schema` and `description` are snapshots from the last
-- registration so the Settings page can show them without having to
-- re-walk the live registry.
CREATE TABLE IF NOT EXISTS config_tool_access (
    tool_name        TEXT    PRIMARY KEY,
    source           TEXT    NOT NULL,        -- "builtin" | "plugin" | "mcp"
    source_id        TEXT,                    -- plugin_id, mcp server_id, or NULL for builtins
    enabled          INTEGER NOT NULL DEFAULT 1,
    allowed_classes  TEXT    NOT NULL DEFAULT '[]', -- JSON array of TrustClass strings
    description      TEXT,
    input_schema     TEXT,                    -- last-known JSON-Schema snapshot
    first_seen_at    INTEGER NOT NULL,
    last_seen_at     INTEGER NOT NULL,
    removed_at       INTEGER                  -- set when the source no longer lists it
);

CREATE INDEX IF NOT EXISTS idx_tool_access_source
    ON config_tool_access(source, source_id);
