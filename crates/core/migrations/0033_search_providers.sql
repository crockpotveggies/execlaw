-- Search-provider registry. The deep-research gather phase + the
-- agent's `web_search` tool both resolve their provider through this
-- table at dispatch time, so an operator can swap DDG (which gets
-- bot-detected aggressively) for SearxNG (self-hosted, no shared
-- rate limit) or a paid API like Brave Search without touching code.
--
-- The previous design had a single `default_search_provider` text
-- column on `config_research` but no per-provider configuration,
-- which left no place to put SearxNG's base URL or Brave's API key.
-- This table is that home.
--
-- Schema choices
--   * `kind` is the PRIMARY KEY because each provider type can only
--     have one row at a time. Want two SearxNG instances? Operator
--     points the single SearxNG row at a load balancer.
--   * `config_json` is per-kind shape. DuckDuckGo: empty. SearxNG:
--     `{"base_url": "..."}`. Brave: `{"api_key": "..."}`. Tavily:
--     same as Brave. Stored as JSON text (not msgpack) so an
--     operator inspecting the DB can read it.
--   * `is_default` boolean column. The dispatcher resolves "the
--     active provider" as `WHERE enabled = 1 AND is_default = 1`.
--     A trigger keeps at most one default at a time.
--   * `enabled` lets the operator keep config rows around without
--     them being eligible for selection (e.g. a Brave row with a
--     stale key, kept around so the key isn't lost on the next
--     enable).

CREATE TABLE IF NOT EXISTS config_search_providers (
    kind         TEXT    PRIMARY KEY,
    enabled      INTEGER NOT NULL DEFAULT 1,
    is_default   INTEGER NOT NULL DEFAULT 0,
    config_json  TEXT    NOT NULL DEFAULT '{}',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_search_providers_default
    ON config_search_providers(is_default) WHERE is_default = 1;

-- Trigger: enforce single-default invariant. Setting `is_default=1`
-- on any row clears `is_default` on every other row in the same
-- transaction so the dispatcher's `WHERE is_default = 1` query
-- always returns at most one row.
CREATE TRIGGER IF NOT EXISTS trg_single_default_provider
AFTER UPDATE OF is_default ON config_search_providers
FOR EACH ROW
WHEN NEW.is_default = 1
BEGIN
    UPDATE config_search_providers
    SET is_default = 0,
        updated_at = NEW.updated_at
    WHERE kind != NEW.kind AND is_default = 1;
END;

CREATE TRIGGER IF NOT EXISTS trg_single_default_provider_insert
AFTER INSERT ON config_search_providers
FOR EACH ROW
WHEN NEW.is_default = 1
BEGIN
    UPDATE config_search_providers
    SET is_default = 0,
        updated_at = NEW.updated_at
    WHERE kind != NEW.kind AND is_default = 1;
END;

-- Seed: DuckDuckGo gets a row at first migration so the dispatcher
-- has a fallback even before the operator visits Settings → Search.
-- Marked default so first-boot installs continue working without
-- operator intervention. INSERT OR IGNORE so re-running the
-- migration on an existing DB is a no-op.
INSERT OR IGNORE INTO config_search_providers
    (kind, enabled, is_default, config_json, created_at, updated_at)
VALUES
    ('duckduckgo', 1, 1, '{}', 0, 0);
