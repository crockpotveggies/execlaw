-- 0031_skill_proposals.sql
--
-- Phase D.1 + D.3 — operator-reviewable skill proposals.
--
-- Two producers write into this table:
--   * Auto-capture worker (Phase C dry-run mode) — produces
--     `proposal_kind = 'new_skill'` rows when the model would
--     create a fresh skill, but the operator wants to review
--     before the row lands in `state_skills`.
--   * Reuse-update worker (Phase D.3) — produces
--     `proposal_kind = 'version_fork'` rows when an existing
--     `stable` skill's recent invocation revealed an improvement.
--     The new body becomes a proposed FORK; on approve it gets
--     written as a new version of the target skill.
--
-- The `state` column drives the operator UI's filter:
--   * pending    — awaiting review
--   * approved   — operator OK'd; promoted_skill_id +
--                  promoted_version_id point at the resulting rows
--   * rejected   — operator declined; row retained for audit
--   * superseded — newer proposal for the same target arrived
--                  before the operator reviewed this one
--
-- Plus: extend config_skills with a `reuse_update_enabled` flag
-- so the operator can opt into the D.3 worker independently of
-- auto-capture (they're separate behaviors with different risk
-- profiles).

CREATE TABLE IF NOT EXISTS state_skill_proposals (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    proposal_kind       TEXT NOT NULL
                        CHECK (proposal_kind IN ('new_skill', 'version_fork')),
    -- For 'version_fork': the existing skill being forked.
    -- For 'new_skill': NULL (no existing skill yet).
    target_skill_id     INTEGER REFERENCES state_skills(id) ON DELETE CASCADE,
    proposed_name       TEXT NOT NULL,
    description         TEXT NOT NULL,
    body_md             TEXT NOT NULL,
    frontmatter_json    TEXT NOT NULL,
    -- Provenance: which run produced this proposal.
    source_run_id       TEXT NOT NULL,
    -- Optional model-generated summary of why the proposal was
    -- produced (e.g. "trajectory revealed a missing verification
    -- step"). Stored as plain text; renders verbatim in the UI.
    trajectory_summary  TEXT,
    tool_calls_observed INTEGER NOT NULL,
    state               TEXT NOT NULL DEFAULT 'pending'
                        CHECK (state IN ('pending', 'approved', 'rejected', 'superseded')),
    -- Set when state transitions to 'approved'.
    promoted_skill_id   INTEGER REFERENCES state_skills(id),
    promoted_version_id INTEGER REFERENCES state_skill_versions(id),
    created_at          INTEGER NOT NULL,
    reviewed_at         INTEGER,
    reviewer            TEXT,
    decision_notes      TEXT
);

CREATE INDEX IF NOT EXISTS idx_state_skill_proposals_state
    ON state_skill_proposals(state, created_at);

CREATE INDEX IF NOT EXISTS idx_state_skill_proposals_target
    ON state_skill_proposals(target_skill_id) WHERE target_skill_id IS NOT NULL;

-- Phase D.3 — reuse-update worker opt-in. Off by default; operator
-- enables via the same Settings surface as auto_capture_enabled.
ALTER TABLE config_skills
    ADD COLUMN reuse_update_enabled INTEGER NOT NULL DEFAULT 0
    CHECK (reuse_update_enabled IN (0, 1));
