-- 0011: Rename config_runner_deployments → config_backends, collapse
-- to one row per purpose.
--
-- The original `config_runner_deployments` table modelled the
-- inference-backend-per-purpose mapping as a generic CRUD list with
-- a synthetic id PK + `is_default` toggle to handle "multiple
-- deployments per purpose, one default." That design leaked into
-- the SPA as a "+ New deployment" button, which was the wrong
-- abstraction: there is exactly ONE backend per purpose, period.
-- The set of purposes is a fixed enum (Standard | Reasoning |
-- Guardrail | VoiceSTT | VoiceTTS), and the operator's job is to
-- edit each one — never to add or remove rows.
--
-- Phase 8.5: rename the table to match the UI vocabulary
-- ("Backends"), drop the unused id / is_default / active columns,
-- and pin `purpose` as the primary key. Schema is migrated by
-- materialising the new table and copying every row that is the
-- `is_default = 1` choice for its purpose. Older rows that were
-- non-default for the same purpose are dropped — the previous UI
-- never let the operator pick a non-default anyway.
--
-- See also docs/runner-design.md for the runner-side vocabulary
-- ("backend" = inference deployment per purpose; "runner" = the
-- per-conversation hot container the control plane spawns
-- automatically).

CREATE TABLE IF NOT EXISTS config_backends (
    purpose             TEXT    PRIMARY KEY,    -- Standard | Reasoning | Guardrail | VoiceSTT | VoiceTTS
    inference_backend   TEXT    NOT NULL,       -- PluginId, e.g. "service-vllm"
    model_spec_json     BLOB    NOT NULL,
    gpu_id              TEXT,
    endpoint            TEXT,
    notes               TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

-- Carry over any default-deployment rows that already exist. We pick
-- one row per purpose: the one flagged is_default = 1; ties broken by
-- earliest created_at. Rows that aren't the default are silently
-- dropped — the UI never let them be selected.
INSERT OR IGNORE INTO config_backends
    (purpose, inference_backend, model_spec_json, gpu_id, endpoint, notes,
     created_at, updated_at)
SELECT purpose, inference_backend, model_spec_json, gpu_id, endpoint, notes,
       created_at, updated_at
FROM config_runner_deployments
WHERE is_default = 1
  AND active = 1
GROUP BY purpose
HAVING MIN(created_at);

DROP TABLE IF EXISTS config_runner_deployments;
