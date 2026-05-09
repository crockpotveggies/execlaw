-- Phase 14.D — repair existing managed-row endpoints that lack
-- the `/v1` suffix.
--
-- The supervisor used to write `http://127.0.0.1:{port}` to
-- `config_backends.endpoint` after a successful health probe.
-- The inference-api client treats that string as the OpenAI
-- base URL and appends `/chat/completions` to it — so the
-- runner POSTs `http://127.0.0.1:8101/chat/completions`, but
-- vLLM only mounts its OpenAI routes under `/v1/...` and the
-- request 404s with `{"detail":"Not Found"}`.
--
-- The supervisor now writes `http://127.0.0.1:{port}/v1` for
-- every Healthy observation; this migration retrofits rows that
-- predate that fix so the operator doesn't have to wait for the
-- next reconcile to write the corrected URL.
--
-- Conservative: we only patch managed rows whose endpoint is a
-- bare loopback URL with no path. External rows (operator-typed
-- URLs) are untouched. If a managed row already carries a path
-- (whether `/v1` or anything else), we leave it alone — the
-- operator may have hand-edited it.
UPDATE config_backends
SET endpoint = endpoint || '/v1',
    updated_at = unixepoch('now')
WHERE mode = 'managed'
  AND endpoint IS NOT NULL
  AND endpoint LIKE 'http://127.0.0.1:%'
  -- A bare loopback URL has exactly two slashes (the `//` after
  -- the protocol). Adding a path means at least three. We use
  -- the three-slash form as the "already has a path" sentinel
  -- so we don't double-suffix a row that already ends `/v1`.
  AND endpoint NOT LIKE '%/%/%/%';
