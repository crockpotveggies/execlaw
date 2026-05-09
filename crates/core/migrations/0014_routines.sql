-- 0014: Routines (cron-shaped agent automations) — §5.6.
--
-- Two tables:
--   * `config_routines` — operator-edited cron schedules + prompts.
--     Audit-logged; bumped `updated_at` on every save.
--   * `state_routine_runs` — append-only run history. Retention
--     sweeper trims runs older than the configured window.
--
-- A routine fires when the wall-clock minute crosses `next_run_at`;
-- the scheduler dispatches the stored prompt as a controller turn on
-- either the configured `target_conversation_id` or a freshly-minted
-- conversation. Trust-class is the controller's; the cron-fired turn
-- gets exactly the same tool-allowlist surface as a normal manual
-- turn — no escalation, no bypass.

CREATE TABLE IF NOT EXISTS config_routines (
    id                       TEXT    PRIMARY KEY,
    name                     TEXT    NOT NULL,
    schedule_cron            TEXT    NOT NULL,    -- 5-field cron
    timezone                 TEXT    NOT NULL DEFAULT 'UTC',
    prompt                   TEXT    NOT NULL,
    -- NULL → mint a fresh conversation each fire. When set, every
    -- run lands on the same conversation (so the agent can reference
    -- prior runs as context).
    target_conversation_id   TEXT,
    enabled                  INTEGER NOT NULL DEFAULT 1,
    last_run_at              INTEGER,
    -- One of "Success" | "Failed" | "Skipped". NULL until first run.
    last_run_status          TEXT,
    -- Computed at save + after every run. Stored in UTC unix epoch
    -- so the scheduler tick is timezone-agnostic.
    next_run_at              INTEGER,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL,
    CHECK (enabled IN (0, 1)),
    CHECK (last_run_status IS NULL
        OR last_run_status IN ('Success', 'Failed', 'Skipped'))
);

CREATE INDEX IF NOT EXISTS idx_config_routines_next_run_at
    ON config_routines(next_run_at)
    WHERE enabled = 1;

CREATE TABLE IF NOT EXISTS state_routine_runs (
    id                  TEXT    PRIMARY KEY,
    routine_id          TEXT    NOT NULL,
    fired_at            INTEGER NOT NULL,    -- when the scheduler decided to fire
    started_at          INTEGER,             -- runner pickup
    finished_at         INTEGER,
    status              TEXT    NOT NULL,    -- Pending | Success | Failed | Skipped
    error               TEXT,
    conversation_id     TEXT,
    FOREIGN KEY (routine_id) REFERENCES config_routines(id) ON DELETE CASCADE,
    CHECK (status IN ('Pending', 'Success', 'Failed', 'Skipped'))
);

CREATE INDEX IF NOT EXISTS idx_state_routine_runs_routine_fired
    ON state_routine_runs(routine_id, fired_at DESC);

CREATE INDEX IF NOT EXISTS idx_state_routine_runs_pending
    ON state_routine_runs(status)
    WHERE status = 'Pending';
