-- 0016: Update default personality voice_id to the locked-decision blend.
--
-- Audit closure: the v1 default seeded in 0013_personality.sql used the
-- single voice 'bf_emma'. The locked-decision (per
-- `feedback/project_locked_decisions_2026_04_23.md`) is the blend
-- 'bf_emma+am_michael' — Kokoro's combined-voice syntax for a
-- mixed-pair output.
--
-- We update only the default-scope row so any per-conversation
-- overrides the operator already set survive. Updating an empty
-- voice_id (NULL or '') would reset operator-typed values, so we
-- match on the prior literal 'bf_emma' too.

UPDATE config_personality
SET    voice_id   = 'bf_emma+am_michael',
       updated_at = unixepoch()
WHERE  scope_kind = 'default'
  AND  scope_ref  = ''
  AND  voice_id   = 'bf_emma';
