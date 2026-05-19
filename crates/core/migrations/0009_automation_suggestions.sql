-- 2026-05-17 — M4 of Automations: discovery surface.
--
-- The `/automations` landing page surfaces three kinds of
-- information about untriaged events:
--
--   1. Aggregate cards (active count, runs 24h, success rate,
--      untriaged count) — computed live from existing tables, no
--      new schema needed.
--   2. **Suggestions** — `(kind, source)` patterns the daily sweep
--      has detected as high-volume + no-matching-automation. The
--      operator reviews and either creates an automation from the
--      template or dismisses the suggestion.
--   3. **Muted patterns** — `(kind, source)` pairs the operator
--      explicitly dismissed. Future sweeps skip these so the same
--      pattern doesn't keep nagging.
--
-- `state_automation_suggestions` carries one row per open suggestion.
-- The `UNIQUE(kind, source, status)` index keeps the sweep idempotent:
-- a re-sweep with the same pattern updates the existing pending row
-- (count, sample_event_ids) rather than spawning duplicates.
--
-- `state_automation_muted_patterns` is keyed on `(kind, source)` so
-- the sweep can short-circuit muted patterns with a single index
-- lookup per candidate.

CREATE TABLE state_automation_suggestions (
    id                TEXT PRIMARY KEY,
    kind              TEXT NOT NULL,
    source            TEXT NOT NULL,
    event_count       INTEGER NOT NULL,
    sample_event_ids  TEXT NOT NULL,         -- JSON [String]
    suggested_name    TEXT NOT NULL,
    status            TEXT NOT NULL,         -- pending | dismissed | actioned
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    UNIQUE(kind, source, status)
);

CREATE INDEX idx_suggestions_status_updated
    ON state_automation_suggestions(status, updated_at);

CREATE TABLE state_automation_muted_patterns (
    kind          TEXT NOT NULL,
    source        TEXT NOT NULL,
    muted_at      INTEGER NOT NULL,
    PRIMARY KEY (kind, source)
);
