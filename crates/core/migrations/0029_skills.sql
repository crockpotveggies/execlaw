-- 0029_skills.sql
--
-- Skill subsystem (Phase A) — DB-backed, versioned, content-
-- addressed procedural-knowledge artifacts the agent loads on
-- demand to shape its behavior. Replaces the inert SkillDecl
-- placeholder in plugin manifests with a first-class store the
-- admin can author into directly via the `skills.create` tool.
--
-- Tables:
--   * state_skills            — one row per skill; pointer to current version
--   * state_skill_versions    — immutable per-version body + metadata
--   * state_blobs             — content-addressed bytes (resources, dedup)
--   * state_skill_resources   — (version, path) -> blob join
--   * state_skill_invocations — runtime activation log (for analytics + audit)
--   * skill_search            — FTS5 index over name/description/body
--
-- Invariants:
--   * skill_versions.version is monotonic per skill (UNIQUE(skill_id, version))
--   * stable skills are never mutated in place — edits create a new
--     version row and advance state_skills.current_version_id
--   * blob refcount is maintained by triggers on state_skill_resources;
--     a blob with refcount == 0 is eligible for GC (vacuum job, future)
--   * FTS index stays in sync via standard content= triggers
--
-- The schema intentionally omits any trust-class gating columns.
-- Skills are advice to the model; the actual tool calls a skill
-- provokes still go through the existing tool dispatcher and its
-- per-tool trust gating. Belt-and-suspenders here would be redundant.

CREATE TABLE IF NOT EXISTS state_skills (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    name                  TEXT    NOT NULL UNIQUE,
    current_version_id    INTEGER REFERENCES state_skill_versions(id),
    state                 TEXT    NOT NULL
                          CHECK (state IN ('trial','stable','archived')),
    source                TEXT    NOT NULL,                  -- 'admin' | 'plugin:<id>' | 'agent:<run_id>'
    registration_kind     TEXT    NOT NULL
                          CHECK (registration_kind IN ('authored','shipped','registered')),
    owning_plugin_id      TEXT,                              -- non-null iff registration_kind != 'authored'
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    archived_at           INTEGER
);

CREATE INDEX IF NOT EXISTS idx_state_skills_state
    ON state_skills(state) WHERE state != 'archived';

CREATE INDEX IF NOT EXISTS idx_state_skills_owning_plugin
    ON state_skills(owning_plugin_id) WHERE owning_plugin_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS state_skill_versions (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_id            INTEGER NOT NULL REFERENCES state_skills(id) ON DELETE CASCADE,
    version             INTEGER NOT NULL,                    -- monotonic per skill, starts at 1
    description         TEXT    NOT NULL,                    -- matcher text shown in skills.list
    body_md             TEXT    NOT NULL,                    -- procedural body
    frontmatter_json    TEXT    NOT NULL,                    -- structured frontmatter
    body_sha256         TEXT    NOT NULL,                    -- content address for dedup + audit
    authored_by         TEXT    NOT NULL,                    -- 'admin:<user_id>' | 'agent:<run_id>' | 'plugin:<id>'
    authored_at         INTEGER NOT NULL,
    promotion_notes     TEXT,
    parent_version_id   INTEGER REFERENCES state_skill_versions(id),
    UNIQUE(skill_id, version)
);

CREATE INDEX IF NOT EXISTS idx_state_skill_versions_skill
    ON state_skill_versions(skill_id);

CREATE TABLE IF NOT EXISTS state_blobs (
    sha256       TEXT PRIMARY KEY,                           -- 64-char hex
    bytes        BLOB NOT NULL,
    mime         TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    refcount     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS state_skill_resources (
    skill_version_id INTEGER NOT NULL REFERENCES state_skill_versions(id) ON DELETE CASCADE,
    path             TEXT    NOT NULL,
    blob_sha         TEXT    NOT NULL REFERENCES state_blobs(sha256),
    PRIMARY KEY (skill_version_id, path)
);

CREATE INDEX IF NOT EXISTS idx_state_skill_resources_blob
    ON state_skill_resources(blob_sha);

-- Refcount maintenance triggers. Insertion bumps; deletion drops.
-- A blob with refcount == 0 is eligible for GC (vacuum job in a
-- future phase); we never auto-DELETE here so the GC pass can be
-- conservative (e.g. retain N days for forensic recovery).
CREATE TRIGGER IF NOT EXISTS trg_state_skill_resources_ai
AFTER INSERT ON state_skill_resources
BEGIN
    UPDATE state_blobs SET refcount = refcount + 1 WHERE sha256 = NEW.blob_sha;
END;

CREATE TRIGGER IF NOT EXISTS trg_state_skill_resources_ad
AFTER DELETE ON state_skill_resources
BEGIN
    UPDATE state_blobs SET refcount = refcount - 1 WHERE sha256 = OLD.blob_sha;
END;

CREATE TABLE IF NOT EXISTS state_skill_invocations (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_id          INTEGER NOT NULL REFERENCES state_skills(id),
    skill_version_id  INTEGER NOT NULL REFERENCES state_skill_versions(id),
    conversation_id   TEXT    NOT NULL,
    loaded_at         INTEGER NOT NULL,
    outcome           TEXT CHECK (outcome IN ('success','failure','aborted')),
    outcome_at        INTEGER,
    tool_calls_made   INTEGER,
    notes             TEXT
);

CREATE INDEX IF NOT EXISTS idx_state_skill_invocations_skill_time
    ON state_skill_invocations(skill_id, loaded_at);

CREATE INDEX IF NOT EXISTS idx_state_skill_invocations_conv
    ON state_skill_invocations(conversation_id);

-- FTS5 index for the optional skills.search router. Contentless
-- shadowing of state_skill_versions; standard content= triggers
-- keep it in sync with the source table.
CREATE VIRTUAL TABLE IF NOT EXISTS skill_search USING fts5(
    description,
    body_md,
    content='state_skill_versions',
    content_rowid='id',
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS trg_state_skill_versions_ai
AFTER INSERT ON state_skill_versions
BEGIN
    INSERT INTO skill_search(rowid, description, body_md)
    VALUES (NEW.id, NEW.description, NEW.body_md);
END;

CREATE TRIGGER IF NOT EXISTS trg_state_skill_versions_ad
AFTER DELETE ON state_skill_versions
BEGIN
    INSERT INTO skill_search(skill_search, rowid, description, body_md)
    VALUES('delete', OLD.id, OLD.description, OLD.body_md);
END;

-- updated_at maintenance: any change to current_version_id or state
-- advances state_skills.updated_at. Keeps the row's last-touched
-- timestamp authoritative for cache invalidation.
CREATE TRIGGER IF NOT EXISTS trg_state_skills_au
AFTER UPDATE OF current_version_id, state ON state_skills
WHEN NEW.updated_at = OLD.updated_at
BEGIN
    UPDATE state_skills SET updated_at = strftime('%s','now') * 1000
    WHERE id = NEW.id;
END;
