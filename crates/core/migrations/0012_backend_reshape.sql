-- 0012: Reshape config_backends — drop Guardrail/Reasoning, add Small,
-- and surface reasoning as a flag on Standard.
--
-- The 5-purpose layout from migration 0011 over-modelled the
-- runner's actual needs:
--
--   * **Guardrail** had no call site. The locked decisions (single
--     `QuantTrio/Qwen3.5-27B-AWQ` model for everything) made a
--     separate Guardrail model architecturally redundant; it was
--     leftover from an earlier multi-model plan. Dropped.
--
--   * **Reasoning** is a *capability* of the Standard model in the
--     post-2026-04-23 single-model world (Qwen3.5 has native
--     reasoning), not a separate deployment. Folded into Standard
--     as a `reasoning_enabled` boolean — the runner reads this on
--     the turn config to opt into <think> blocks.
--
--   * **Small** is added: a fast-path backend for voice mode (where
--     latency dominates UX) and any other "do this turn quick"
--     route. The runner falls back to Standard when Small isn't
--     configured.
--
-- The result is a four-purpose layout — Standard, Small, VoiceSTT,
-- VoiceTTS — each row optionally configurable.

ALTER TABLE config_backends
    ADD COLUMN reasoning_enabled INTEGER NOT NULL DEFAULT 0;

-- Best-effort carry-over: if the operator had a `Reasoning` row
-- configured, copy its inference_backend / endpoint / gpu_id into
-- the Standard row and flip reasoning_enabled = 1. Standard wins on
-- conflict (its existing config is what the runner uses today).
UPDATE config_backends
SET reasoning_enabled = 1
WHERE purpose = 'Standard'
  AND EXISTS (
      SELECT 1 FROM config_backends r
      WHERE r.purpose = 'Reasoning'
  );

-- Drop deprecated rows. Operator policy that referenced them is
-- gone; the four remaining purposes have their own rows or remain
-- unconfigured.
DELETE FROM config_backends
WHERE purpose IN ('Guardrail', 'Reasoning');
