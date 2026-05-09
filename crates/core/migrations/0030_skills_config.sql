-- 0030_skills_config.sql
--
-- Singleton-row config table for the skill subsystem (Phase C).
-- Mirrors the `config_general` pattern (one row, primary key 1
-- forced by CHECK, INSERT OR IGNORE seeds defaults).
--
-- Columns:
--   * auto_capture_enabled        — operator opt-in flag for the
--     auto-capture worker. Default OFF (0). Locked design decision
--     2026-05-02: silent learning surprises operators, so the worker
--     stays dormant until the operator explicitly turns it on.
--   * auto_capture_min_tool_calls — threshold below which a turn
--     is not summarized into a draft skill. Default 5 (matches the
--     Hermes-research baseline; trajectories with <5 tool calls
--     rarely generalize into reusable procedures).
--   * auto_capture_dry_run        — when 1, the worker runs the
--     full pipeline (replay + sanitize + summarize) but DOES NOT
--     write a skill row. Used to evaluate the worker's quality on
--     a live operator's traffic without committing drafts.
--
-- Future Phase C additions (reuse-update worker, eval window) will
-- ALTER TABLE this same row rather than spawning a second config
-- table — the singleton pattern is sufficient.

CREATE TABLE IF NOT EXISTS config_skills (
    id                          INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    auto_capture_enabled        INTEGER NOT NULL DEFAULT 0
                                CHECK (auto_capture_enabled IN (0, 1)),
    auto_capture_min_tool_calls INTEGER NOT NULL DEFAULT 5
                                CHECK (auto_capture_min_tool_calls >= 1
                                       AND auto_capture_min_tool_calls <= 100),
    auto_capture_dry_run        INTEGER NOT NULL DEFAULT 0
                                CHECK (auto_capture_dry_run IN (0, 1)),
    updated_at                  INTEGER NOT NULL
);

INSERT OR IGNORE INTO config_skills
    (id, auto_capture_enabled, auto_capture_min_tool_calls, auto_capture_dry_run, updated_at)
VALUES (1, 0, 5, 0, strftime('%s','now') * 1000);
