-- 0017: General settings (Phase 14).
--
-- Single-row "general settings" table for operator-editable knobs that
-- don't fit any of the existing per-feature tables (Backends,
-- Personality, Trust policy, etc.). v1 ships:
--
--   * start_on_boot — does the host service launch at OS boot?
--                     Stored in the DB so the SPA can render the
--                     toggle; the actual "register at boot" flag
--                     is set when `execlaw service install` runs
--                     (service-manager honours autostart=true on
--                     all backends). The SPA toggle on this row
--                     re-runs the service registration.
--
--   * bind_address  — host:port the service listens on. Default
--                     loopback:3031. Editing this saves to the DB
--                     and prompts the operator to re-run
--                     `execlaw service restart` (the binary reads
--                     this on next start).
--
-- Future fields (data dir, log level, telemetry opt-in, …) land via
-- ALTER TABLE in subsequent migrations.

CREATE TABLE IF NOT EXISTS config_general (
    -- Singleton row guard. The CHECK forces every INSERT to use 1 so
    -- the table can never grow beyond a single row.
    id              INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    start_on_boot   INTEGER NOT NULL DEFAULT 1 CHECK (start_on_boot IN (0, 1)),
    bind_address    TEXT    NOT NULL DEFAULT '127.0.0.1:3031',
    updated_at      INTEGER NOT NULL
);

-- Seed the singleton row so /api/admin/settings/general always
-- returns a payload without a per-route NULL fallback.
INSERT OR IGNORE INTO config_general (id, start_on_boot, bind_address, updated_at)
VALUES (1, 1, '127.0.0.1:3031', unixepoch());
