-- 0015: Add `mode` column to config_backends for managed-vs-external
-- backend lifecycle (Phase 12 — §5.4 / §5).
--
-- Semantics:
--   * `external` (default, all existing rows) — operator points
--     execlaw at an endpoint they run themselves (e.g. an existing
--     vLLM at http://192.168.1.50:8000/v1). Control plane never
--     touches the process; `endpoint` column is operator-supplied.
--   * `managed` — control plane spawns + supervises the inference
--     service container itself. Operator picks image tag + cmd args
--     in `model_spec_json`; `endpoint` is computed at spawn time and
--     written back so the runner has a URL to call. Health checks +
--     restart policy live in the BackendSupervisor (server crate).
--
-- The default of `external` keeps every pre-Phase-12 row's behaviour
-- identical — the new logic only kicks in when an operator explicitly
-- flips a row to managed in Settings → Backends.

ALTER TABLE config_backends
    ADD COLUMN mode TEXT NOT NULL DEFAULT 'external';

-- SQLite doesn't support adding a CHECK to an existing column via
-- ALTER. The application layer enforces the enum on every read/write
-- (BackendMode::parse rejects unknown strings), so missing CHECK is
-- a known-acceptable trade for the migration to apply atomically.
-- A future schema-rebuild migration could add it via the
-- copy-into-new-table dance if we ever need DB-level enforcement.
