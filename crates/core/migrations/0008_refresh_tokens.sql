-- 0008: Persistent refresh-token store (Phase 7 hardening continuation).
--
-- Until now, refresh tokens lived in an in-memory DashMap on the
-- server process. That meant a restart silently signed every user
-- out — fine for a single-user dev setup, but wrong as soon as the
-- server is supposed to survive a deploy without forcing every
-- operator (and every plugin holding a long-lived token) to log
-- back in.
--
-- Each row is one issued refresh token. Tokens are single-use —
-- `consume` deletes the row, and an attempted reuse returns None.
-- Logout reads the row to learn the session_id, then calls
-- `revoke_session` which deletes every row sharing that
-- session_id (handles the rotated-and-not-yet-consumed case).
--
-- "Logout everywhere" deletes every row for a `principal_id`,
-- which atomically signs the user out of every browser they have
-- open.
--
-- `expires_at` is unix seconds. We don't bother with a periodic
-- sweeper today — the consume path checks expiry, and orphaned
-- expired rows are tiny. A future sweeper can DELETE WHERE
-- expires_at < ?strftime('%s','now')? if the table ever grows.
CREATE TABLE IF NOT EXISTS state_refresh_tokens (
    token         TEXT    PRIMARY KEY,        -- the opaque refresh token string
    principal_id  TEXT    NOT NULL,           -- user_id; same value embedded in JWT `sub`
    session_id    TEXT    NOT NULL,           -- shared across rotations within one login
    issued_at     INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_refresh_tokens_session_id
    ON state_refresh_tokens(session_id);
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_principal_id
    ON state_refresh_tokens(principal_id);
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_expires_at
    ON state_refresh_tokens(expires_at);
