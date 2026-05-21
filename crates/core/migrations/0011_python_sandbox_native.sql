-- 2026-05-20 — migration: python-sandbox transitions from plugin to
-- native feature.
--
-- Background: python-sandbox shipped as a plugin (state_plugins row,
-- `[[ui_panels]]` config panel, vault-stored settings, manifest-driven
-- sidecar). That was the wrong architecture — the implementation is
-- tightly coupled to the host crate (per-conversation kernel pool,
-- Jupyter WS protocol, 5,685 LOC of Rust in crates/server/src/python_sandbox/)
-- and the SDK doesn't support shipping Rust. The grounding-rule
-- violation the operator caught was real: the host crate had no
-- business pretending to be a plugin host for this feature.
--
-- The new model:
--   * Native feature with its own Settings page (`/settings/python-sandbox`).
--   * Native config table — this migration creates `config_python_sandbox`.
--   * Native sidecar lifecycle — registered via the host's own boot
--     code, not via plugin manifest discovery.
--   * Native enable toggle that disables tools + sidecar when off.
--   * Auto-disabled on boot when Docker is unavailable (Apple Silicon
--     without Docker Desktop, etc.).
--
-- This migration:
--   1. Creates the `config_python_sandbox` table (single-row config,
--      same pattern as `config_general`). Defaults: disabled, 900s
--      idle timeout, 50 MiB max output.
--   2. Drops the legacy plugin row from `state_plugins` if present,
--      so the boot path doesn't try to enable a phantom plugin.
--   3. Cleans up the legacy plugin-scoped vault rows where settings
--      used to live — values get re-entered via the new Settings
--      page if the operator wants them.
--   (Sidecar supervisor state is in-memory; nothing to clean
--    there. The existing kernel-gateway container on disk gets
--    adopted or respawned on next boot via the supervisor's
--    standard reconcile pass.)

CREATE TABLE IF NOT EXISTS config_python_sandbox (
    -- Single-row table. `id = 1` is the only row; the get/set helpers
    -- enforce this. Same trick as config_general so updates are a
    -- simple `INSERT OR REPLACE … VALUES (1, …)`.
    id                       INTEGER PRIMARY KEY CHECK (id = 1),

    -- Master toggle. Off by default on fresh installs — operator
    -- opts in via the Settings page. Also flipped to 0 on boot
    -- when Docker is unreachable (no point keeping it "on" when
    -- the sidecar can't possibly spawn).
    enabled                  INTEGER NOT NULL DEFAULT 0,

    -- Operator-tunable knobs that used to live in the plugin-
    -- settings vault under (plugin_id='python-sandbox', name='...').
    -- Bounded server-side (60..86400 / 1MiB..500MiB) — the same
    -- ranges the previous plugin panel enforced.
    idle_timeout_seconds     INTEGER NOT NULL DEFAULT 900,
    max_output_bytes         INTEGER NOT NULL DEFAULT 52428800,

    updated_at               INTEGER NOT NULL DEFAULT 0
);

-- Seed the single config row with defaults so the get helper
-- can always return Ok(config) without a NULL check.
INSERT OR IGNORE INTO config_python_sandbox (id, enabled, idle_timeout_seconds, max_output_bytes, updated_at)
VALUES (1, 0, 900, 52428800, 0);

-- Drop the legacy plugin row + any plugin-scoped settings if the
-- operator had the plugin installed under the old architecture.
-- IF NOT EXISTS clauses are unnecessary — DELETE is always
-- idempotent against missing rows.
DELETE FROM state_plugins WHERE plugin_id = 'python-sandbox';
DELETE FROM vault_secrets WHERE plugin_id = 'python-sandbox';

-- Sidecar runtime state is in-memory (the SidecarSupervisor
-- doesn't persist its slot map), so there's nothing else to
-- clean up here. The previously-spawned kernel-gateway container
-- on disk is left alone — the boot path will inspect, adopt or
-- re-spawn it as needed.
