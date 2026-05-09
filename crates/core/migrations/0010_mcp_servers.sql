-- 0010: External MCP servers (Phase 8c).
--
-- One row per MCP endpoint the operator has configured. The Phase-8b
-- `mcp-client` crate connects to each enabled row on boot, runs the
-- initialise handshake, and reflects the server's `tools/list`
-- result into `config_tool_access` rows tagged with
-- `source = 'mcp'`, `source_id = <this id>`, and tool_name prefixed
-- `mcp:<id>:<remote_name>`.
--
-- `id` is an operator-chosen slug (alphanumeric, hyphen, underscore;
-- must be unique). Used in tool-name prefixes and audit rows.
--
-- `transport` is "stdio" today. Phase 8c adds the column shape for
-- "streamable_http" but the connection manager only wires stdio in
-- this migration; the HTTP rows are persisted but ignored at boot
-- with a warning until the HTTP transport ships in 8c-followup.
--
-- `default_allowed_classes` is the trust-class allowlist applied to
-- tools discovered on this server FOR THE FIRST TIME. Per the
-- locked decision (6c), per-tool overrides via Settings → Tools
-- take precedence; the server-level default is just the seed for
-- new rows.
--
-- `auth_secret_ref` is a vault key (resolved at connect time) for
-- HTTP transports. Stdio runs as the operator's process so it has
-- no auth needs at the protocol layer.
CREATE TABLE IF NOT EXISTS config_mcp_servers (
    id                       TEXT    PRIMARY KEY,                -- slug
    display_name             TEXT    NOT NULL,
    transport                TEXT    NOT NULL,                   -- "stdio" | "streamable_http"
    -- stdio-specific:
    command                  TEXT,
    args_json                TEXT,                               -- JSON array
    env_json                 TEXT,                               -- JSON map (kept opaque)
    cwd                      TEXT,
    -- http-specific (Phase 8c follow-up):
    url                      TEXT,
    auth_secret_ref          TEXT,
    -- common:
    enabled                  INTEGER NOT NULL DEFAULT 1,
    default_allowed_classes  TEXT    NOT NULL DEFAULT '["Controller"]', -- JSON array
    status                   TEXT,                               -- "connected" | "disconnected" | "error"
    last_error               TEXT,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled
    ON config_mcp_servers(enabled);
