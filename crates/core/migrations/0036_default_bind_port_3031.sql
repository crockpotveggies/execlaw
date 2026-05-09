-- 0036: Flip default bind port from 3030 to 3031.
--
-- Migration 0017 originally seeded `config_general.bind_address` as
-- `127.0.0.1:3030`. The default was retroactively flipped to
-- `127.0.0.1:3031` to align dev-mode and production paths (the dev
-- server steers off :3030 to dodge Docker Desktop's vpnkit on
-- Windows; once that became the default everywhere the production
-- service followed). Editing 0017 in place would break checksum
-- verification for every existing install — this migration honors
-- the append-only invariant by leaving 0017 byte-stable and applying
-- the change as a discrete event.
--
-- Behaviour:
--
--   * Fresh installs that hit 0017 *and* 0036 in sequence land on
--     :3031 (the UPDATE below replaces the seeded :3030 row).
--   * Existing installs that already had a custom bind_address
--     (anything other than the seeded default) are LEFT UNCHANGED —
--     the WHERE clause specifically targets the seeded value to
--     avoid stomping operator-edited config.
--   * The schema-level DEFAULT on the column is also flipped so
--     future ALTER-TABLE-ADD-COLUMN-style migrations that re-seed
--     a row pick up :3031.

UPDATE config_general
SET bind_address = '127.0.0.1:3031'
WHERE id = 1
  AND bind_address = '127.0.0.1:3030';

-- SQLite supports altering a column's DEFAULT only via the
-- table-rebuild pattern. The DEFAULT is cosmetic at this point
-- (the singleton row is already inserted by 0017's seed) and the
-- in-tree code paths don't rely on the schema-level default —
-- the runtime fallback in `crates/cli/src/main.rs::resolve_bind`
-- is the load-bearing default. Skip the table rebuild; document
-- the schema-default drift here for future readers.
