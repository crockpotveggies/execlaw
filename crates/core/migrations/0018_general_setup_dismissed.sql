-- 0018: Track setup-wizard dismissal in config_general (Phase 14).
--
-- The first-run wizard at `/setup` walks the operator through:
--
--   1. Account creation (POST /api/setup)
--   2. Docker preflight
--   3. Backend setup — pick a serving target + model OR enter an
--      OpenAI-compatible URL
--
-- The completed-or-not signal `/api/ping` uses needs to capture
-- step 3. Two equivalent "complete" outcomes:
--
--   * Standard backend row exists → wizard delivered a working
--     inference target. Server reads `config_backends`.
--   * Operator clicked "Skip for now" on the backend step →
--     they accept that no backend is configured but want the
--     wizard out of the way. Persisted here as
--     `setup_wizard_dismissed_at`.
--
-- Without this column, refreshing mid-wizard or navigating to
-- /chat directly would route around the unfinished setup, which
-- is the bug this migration fixes.

ALTER TABLE config_general
    ADD COLUMN setup_wizard_dismissed_at INTEGER;
