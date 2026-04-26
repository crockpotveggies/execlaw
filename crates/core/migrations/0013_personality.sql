-- 0013: Personality & system-prompt composition (§5.5).
--
-- The operator-editable half of the agent's system prompt: name, tone,
-- custom instructions, persona, voice id. The other half (trust-class
-- rules, refusal behaviour) is built-in and not stored here.
--
-- Two scopes ship in v1:
--   * 'default'      — exactly one row, scope_ref = ''. The fallback
--                      for every conversation. Seeded by this migration
--                      so first-run conversations have a deterministic
--                      prompt before the operator edits anything.
--   * 'conversation' — sparse, scope_ref = conversation_id. Optional
--                      per-conversation overrides; absent rows fall
--                      through to default.
--
-- Composition is field-by-field: an override that only sets `tone`
-- inherits every other field from the default. The composer lives in
-- crates/core/src/personality.rs::compose_system_prompt.
--
-- A future per-transport-group scope is reachable by extending the
-- scope_kind CHECK without a schema change.

CREATE TABLE IF NOT EXISTS config_personality (
    scope_kind          TEXT    NOT NULL,
    scope_ref           TEXT    NOT NULL,
    display_name        TEXT    NOT NULL DEFAULT '',
    role                TEXT    NOT NULL DEFAULT '',
    tone                TEXT    NOT NULL DEFAULT '',
    communication_style TEXT    NOT NULL DEFAULT '',
    initiative          TEXT    NOT NULL DEFAULT '',
    about_agent         TEXT    NOT NULL DEFAULT '',
    about_controller    TEXT    NOT NULL DEFAULT '',
    custom_instructions TEXT    NOT NULL DEFAULT '',
    voice_id            TEXT,
    -- For overrides: a JSON array of field names that the operator
    -- explicitly set on this scope. The composer treats fields NOT in
    -- this list as "not in override" (use base) instead of "blank in
    -- override" (force blank). Default-scope rows ignore this column —
    -- every field is implicitly present at the default level.
    override_fields_json TEXT NOT NULL DEFAULT '[]',
    version             INTEGER NOT NULL DEFAULT 1,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (scope_kind, scope_ref),
    CHECK (scope_kind IN ('default', 'conversation')),
    CHECK (scope_kind <> 'default' OR scope_ref = '')
);

-- Seed the default-scope row so /api/admin/personality always returns
-- something and compose_system_prompt never returns the empty string.
-- Conservative defaults: identity-only, no tone/style assertions, so
-- the agent reads as plain and helpful until the operator edits it.
INSERT OR IGNORE INTO config_personality
    (scope_kind, scope_ref, display_name, role, tone, communication_style,
     initiative, about_agent, about_controller, custom_instructions,
     voice_id, override_fields_json, version, created_at, updated_at)
VALUES
    ('default', '', 'execlaw', 'Personal assistant', '', '',
     '', '', '', '',
     'bf_emma', '[]', 1,
     unixepoch(), unixepoch());
