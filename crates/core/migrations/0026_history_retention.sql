-- 2026-04-29 — global history-retention setting.
--
-- Default 30 days. Allowed values are 0 (= infinite, never delete) /
-- 30 / 60 / 90 / 120; the API layer enforces the option set so a
-- malformed Settings save can't write some random number that would
-- silently fall through every sweeper's "is this still in retention"
-- check.
--
-- Sweepers consume this via `RetentionPolicy::current_days()`:
--   * 0  → return None (skip the tick — keep forever)
--   * N  → return cutoff = now - N*86_400, delete rows older than it
--
-- Subject to retention: `state_events`, terminal `state_research_jobs`
-- (lands in PR 6), `state_routine_runs`, resolved/acked `state_alerts`,
-- structured logs.
--
-- NOT subject (intentional carve-outs):
--   * `memory_entries` — durable by design. Per-key TTL is the right
--     opt-in mechanism for entries that should expire.
--   * audit log — separate compliance retention.
--   * `state_refresh_tokens` — security artifact, has its own sweeper
--     bound to JWT TTL semantics.
--   * `vault_secrets` — never expire automatically.

ALTER TABLE config_general
    ADD COLUMN history_retention_days INTEGER NOT NULL DEFAULT 30;
