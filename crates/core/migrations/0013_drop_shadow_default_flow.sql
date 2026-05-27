-- Migration 0013 — drop the lingering "(shadow)" web flow row.
--
-- An earlier (slice 7) build seeded the default web prompt flow with
-- name `Default web prompt flow (shadow)` and `enabled = 0`. The
-- cutover commit renamed it to `Default web prompt flow` + enabled,
-- but for installs that had ALREADY booted the shadow build the row
-- persisted under its old name. This migration drops any row that
-- still uses the legacy name AND is disabled — operator-authored
-- rows that happened to share the name don't get clobbered (the
-- enabled flag filters those out; operators don't ship disabled
-- "(shadow)" rows by hand).

DELETE FROM state_automations
 WHERE name = 'Default web prompt flow (shadow)'
   AND enabled = 0;
