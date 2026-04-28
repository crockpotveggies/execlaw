-- Phase 14 follow-up — bump existing managed-vLLM rows whose
-- `model_spec_json.image` still points at the locked-in
-- `vllm/vllm-openai:v0.6.2` tag we used pre-Qwen-3.5.
--
-- Why: v0.6.2 pre-dates the Qwen 3.5 architecture in vLLM's model
-- registry. A managed Standard backend with `--model=
-- QuantTrio/Qwen3.5-27B-AWQ` against v0.6.2 exits with `unknown
-- architecture: Qwen3_5MoEForCausalLM` immediately after the model
-- weights download — the supervisor catches that as CrashLooping
-- but the operator only sees "container died" without a clear next
-- step. The wizard now writes `vllm/vllm-openai:nightly` for fresh
-- saves; this migration fixes rows the wizard wrote before the
-- bump.
--
-- The repair is conservative — it ONLY rewrites rows whose image
-- field exactly matches the legacy `v0.6.2` pin. Operators who
-- intentionally hand-edited the JSON to lock to a specific newer
-- digest, or who switched to OpenVINO / OpenArc, are untouched.
--
-- Idempotent: rerunning is a no-op because the WHERE clause filters
-- on the exact legacy string.
UPDATE config_backends
SET model_spec_json = json_set(
        model_spec_json,
        '$.image',
        'vllm/vllm-openai:nightly'
    )
WHERE mode = 'managed'
  AND json_extract(model_spec_json, '$.image') = 'vllm/vllm-openai:v0.6.2';
