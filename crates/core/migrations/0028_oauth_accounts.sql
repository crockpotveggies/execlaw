-- 0028_oauth_accounts.sql
--
-- Generic OAuth 2.0 client + token storage for plugin-declared
-- [[oauth_accounts]] entries (manifest schema in
-- crates/plugin-sdk/src/manifest.rs OauthAccountDecl). Keyed by
-- (plugin_id, account_name) so a plugin can declare multiple
-- accounts (e.g. plugin-google-calendar's "controller" + "team").
--
-- Two tables, three states:
--   * No row in either table        -> operator hasn't configured the plugin's OAuth client yet
--   * state_oauth_clients only      -> client credentials saved, "Connect Account" pending
--   * state_oauth_clients + tokens  -> connected; tokens auto-refresh via the sweeper
--
-- A third table, state_oauth_pending, is the short-lived CSRF
-- state during the authorize redirect. Sweeper purges expired
-- entries on a 5-minute cadence.
--
-- All sensitive blobs (client_secret, refresh_token, access_token)
-- live in regular columns; the database file is SQLCipher-encrypted
-- at rest, matching how vault_secrets stores its blobs.

CREATE TABLE IF NOT EXISTS state_oauth_clients (
    plugin_id      TEXT    NOT NULL,
    account_name   TEXT    NOT NULL,
    provider       TEXT    NOT NULL,                  -- 'google'
    client_id      TEXT    NOT NULL,
    client_secret  TEXT    NOT NULL,
    redirect_uri   TEXT    NOT NULL,
    scopes_json    TEXT    NOT NULL,                  -- JSON array of OAuth scopes (manifest-declared)
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, account_name)
);

CREATE TABLE IF NOT EXISTS state_oauth_tokens (
    plugin_id          TEXT    NOT NULL,
    account_name       TEXT    NOT NULL,
    access_token       TEXT    NOT NULL,
    refresh_token      TEXT,                          -- nullable: some providers don't issue one on re-consent
    token_expires_at   INTEGER NOT NULL,              -- unix seconds
    scopes_granted     TEXT    NOT NULL,              -- JSON array of scopes the user actually granted
    account_email      TEXT,                          -- discovered from the token (Google: id_token sub)
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, account_name),
    FOREIGN KEY (plugin_id, account_name)
        REFERENCES state_oauth_clients(plugin_id, account_name)
        ON DELETE CASCADE
);

-- Sweeper indexes the column the refresh background pass orders by.
CREATE INDEX IF NOT EXISTS idx_state_oauth_tokens_expiry
    ON state_oauth_tokens(token_expires_at);

-- Short-lived CSRF state: minted at /authorize, consumed at /callback.
-- Purged on TTL by the same sweeper that refreshes tokens.
CREATE TABLE IF NOT EXISTS state_oauth_pending (
    state_token   TEXT    PRIMARY KEY,                -- random 32-byte URL-safe blob
    plugin_id     TEXT    NOT NULL,
    account_name  TEXT    NOT NULL,
    redirect_to   TEXT,                                -- optional SPA URL to bounce back to after connect
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL                    -- typically created_at + 600 (10 min)
);

CREATE INDEX IF NOT EXISTS idx_state_oauth_pending_expiry
    ON state_oauth_pending(expires_at);
