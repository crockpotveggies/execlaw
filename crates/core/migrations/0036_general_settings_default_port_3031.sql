-- 0036: bump the default bind_address from 3030 → 3031.
--
-- Background: commit 1c81b08 ("align default bind port to
-- 127.0.0.1:3031 everywhere") edited migration 0017 in place to flip
-- the default port. That broke every operator with an already-applied
-- 0017 — the migration runner refuses to continue when an existing
-- id has a different checksum than the file on disk. Migrations are
-- immutable once shipped; the right fix for "change a default" is a
-- new migration.
--
-- This restores 0017 to its original content (port 3030) and applies
-- the port bump here instead, with semantics that match what 1c81b08
-- intended: fresh installs land on 3031, operators who already
-- changed their `bind_address` away from the original default keep
-- their value untouched.
--
-- Why a guarded UPDATE instead of changing the column DEFAULT: SQLite
-- doesn't support ALTER COLUMN to change a DEFAULT clause, and the
-- column DEFAULT is only consulted on an INSERT that omits the value
-- — which never happens here because 0017 always seeds with an
-- explicit value. Updating the row is the only thing that actually
-- moves the needle for live installs, and the WHERE clause is the
-- "leave existing operators alone" guard.

UPDATE config_general
   SET bind_address = '127.0.0.1:3031'
 WHERE id = 1
   AND bind_address = '127.0.0.1:3030';
