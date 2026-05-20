-- 2026-05-17 — M2 of Automations: persistent automation definitions
-- + run history.
--
-- `state_automations` stores the AutomationDef as a JSON blob in
-- `definition` (trigger + nodes + edges). The shape is owned by
-- `crate::automations::AutomationDef`; SQLite is opaque to it. We
-- index on `(enabled, json_extract(definition, '$.trigger.kind'))`
-- so the matcher's lookup at dispatch time is cheap — it filters by
-- enabled + kind before deserializing the JSON for the more specific
-- match predicate.
--
-- `state_automation_runs` stores one row per (automation, triggering
-- event) pair. `step_traces` is a JSON array of per-node trace
-- records — each carries the resolved input, the node's output, ms,
-- and optional error. Together, the trace is the audit log of what
-- happened during that run.
--
-- Soft references only (no enforced FKs):
--   * `automation_id` → `state_automations.id`. Deleting the
--     automation MUST NOT cascade-delete its run history (audit
--     trail). The id stays as a soft ref; the runs UI shows
--     "(deleted)" when the row is gone.
--   * `event_id` → `state_bus_events.id`. The bus retention sweep
--     purges old dispatched events, but their runs should remain
--     visible — the run carries enough payload context in
--     `step_traces` to be useful on its own.
--
-- 2026-05-17 (M2 of Automations).

CREATE TABLE state_automations (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    definition  TEXT NOT NULL,            -- JSON: AutomationDef
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Matcher hot path: SELECT id, definition FROM state_automations
-- WHERE enabled = 1 AND json_extract(definition, '$.trigger.kind') = ?.
-- The expression index lets SQLite avoid scanning every row to find
-- the kind for matching.
CREATE INDEX idx_automations_enabled_trigger_kind
    ON state_automations(enabled, json_extract(definition, '$.trigger.kind'));

CREATE TABLE state_automation_runs (
    id            TEXT PRIMARY KEY,
    automation_id TEXT NOT NULL,           -- soft ref to state_automations.id
    event_id      TEXT NOT NULL,           -- soft ref to state_bus_events.id
    status        TEXT NOT NULL,           -- pending|running|success|failed|skipped
    step_traces   TEXT NOT NULL,           -- JSON [{node_id, input, output, ms, error?}]
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER
);

-- Per-automation run history page is `SELECT ... WHERE automation_id = ?
-- ORDER BY started_at DESC`. The index covers both predicates.
CREATE INDEX idx_automation_runs_started
    ON state_automation_runs(automation_id, started_at);

-- Failure-rate / health rollup needs `WHERE status = ? AND started_at > ?`
-- for the `/automations` page's success-rate card.
CREATE INDEX idx_automation_runs_status_started
    ON state_automation_runs(status, started_at);
