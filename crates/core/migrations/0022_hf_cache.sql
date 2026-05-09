-- Phase 14.C — host-side HuggingFace cache.
--
-- Adds:
--   * `config_general.hf_secondary_caches_json` — operator-supplied
--     list of additional HF cache directories the host-side
--     downloader scans before pulling from huggingface.co. Stored
--     as a JSON array of absolute paths. Empty / NULL means "no
--     secondary caches" (the default). The Settings → Backends
--     "Hugging Face cache" section under Hardware writes this
--     column.
--
-- Backfill rationale: existing managed rows in `config_backends`
-- don't yet carry a `mounts` array in their `model_spec_json`. The
-- supervisor's `attach_hf_cache_mount()` adds the mount at runtime
-- so old rows pick up the new cache as soon as the new binary
-- starts — no per-row migration is required for the mount itself.
-- This keeps the migration small + cheap; complex inline JSON
-- mutation is avoided.

ALTER TABLE config_general
ADD COLUMN hf_secondary_caches_json TEXT;
