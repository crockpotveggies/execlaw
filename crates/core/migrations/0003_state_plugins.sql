-- 0003: Persisted plugin installs (§4, Phase 2).
--
-- The operator installs a plugin via `POST /api/admin/plugins/install`
-- (ZIP upload). The server stages the ZIP, parses the manifest,
-- resolves conflicts against the hook registry, and — on success —
-- writes a row here so the install survives restart.
--
-- On boot, `PluginHost::hydrate` reads every `enabled = 1` row,
-- re-registers its hooks, and (for subprocess-tier plugins) respawns
-- the child process.
--
-- `stage_path` is the on-disk directory that holds the extracted ZIP
-- contents (under `~/.execlaw/plugins/<plugin_id>-<version>/`). It's
-- kept even when a plugin is disabled so re-enable is a no-op for
-- filesystem state.
CREATE TABLE IF NOT EXISTS state_plugins (
    plugin_id     TEXT    PRIMARY KEY,
    version       TEXT    NOT NULL,
    manifest_toml TEXT    NOT NULL,        -- verbatim plugin.toml, for audit
    stage_path    TEXT    NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    installed_at  INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_state_plugins_enabled
    ON state_plugins(enabled);
