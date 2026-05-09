-- Phase 14 follow-up — repair `config_backends` rows whose `gpu_id`
-- holds the legacy full-`GpuId` string (e.g.
-- `"0x10de:PCI\\VEN_10DE&DEV_2230&SUBSYS_…&REV_A1\\…"`) that the
-- pre-fix SetupWizard saved.
--
-- nvidia-container-cli rejects anything that isn't a small ordinal
-- (`"0"`, `"1"`) or a CUDA UUID — the legacy value made every
-- container spawn fail with `unknown device`, which the supervisor
-- correctly classified as CrashLooping but the operator had no way
-- to recover from short of clearing + re-saving the row.
--
-- The repair: any non-NULL `gpu_id` that contains a `\` (the WMI
-- PNP shape ALWAYS has backslashes) or a `:` (the
-- `<vendor_hex>:<device_id>` shape from `GpuId(format!("{}:{}",
-- …))`) gets replaced with `"0"`. This is the per-vendor ordinal
-- of the FIRST device of any vendor on the host — correct for the
-- >99% case of a single-GPU box, and still recoverable on a
-- multi-GPU host because the operator can re-save through the
-- wizard which now writes the right ordinal.
--
-- Idempotent: rows already holding `"0"` / `"1"` / `null` /
-- `"GPU-…"` (CUDA UUID) are untouched. We use `instr()` instead of
-- `LIKE` to dodge SQLite's wildcard-vs-escape semantics, which made
-- earlier drafts of this migration accidentally no-op.
UPDATE config_backends
SET gpu_id = '0'
WHERE gpu_id IS NOT NULL
  AND gpu_id NOT LIKE 'GPU-%'
  AND (instr(gpu_id, char(92)) > 0      -- char(92) = '\'
       OR instr(gpu_id, ':') > 0);
