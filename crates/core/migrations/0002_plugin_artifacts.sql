-- Plugin-rendered artifacts (charts, generated images, future plugin-produced
-- files). Builds on the existing state_artifacts table, which was originally
-- introduced for the deep-research PDF pipeline but had no provision for
-- plugin-scoped attribution or time-to-live.
--
-- Adds three columns:
--   * plugin_id  — which plugin minted the row (NULL for legacy/research rows)
--   * filename   — operator-facing filename (browser save-as, transport file
--                  attachment name); kept separate from `path` because `path`
--                  is the sha256-named blob on disk.
--   * expires_at — unix-seconds wall-clock TTL. NULL means no TTL (research
--                  reports). The ephemeral sweeper culls rows past this stamp
--                  along with their on-disk blob when refcount drops to zero.
--
-- Two indexes — one for the "list a plugin's artifacts" admin path, one for
-- the sweeper's "what's expired now" scan (partial index, so the cost is
-- proportional to ttl'd rows only).

ALTER TABLE state_artifacts ADD COLUMN plugin_id  TEXT;
ALTER TABLE state_artifacts ADD COLUMN filename   TEXT;
ALTER TABLE state_artifacts ADD COLUMN expires_at INTEGER;

CREATE INDEX idx_artifacts_plugin
    ON state_artifacts(plugin_id, created_at)
    WHERE plugin_id IS NOT NULL;

CREATE INDEX idx_artifacts_expiry
    ON state_artifacts(expires_at)
    WHERE expires_at IS NOT NULL;
