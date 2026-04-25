-- 0006: Operator-side `users` table (Phase 6 prep).
--
-- Distinct from `principals` — `principals` are every participant
-- the agent talks to (the controller, plus every cold-contact /
-- KnownTrusted resolved on inbound). `users` are operator-side
-- accounts that authenticate against the SPA + admin API.
--
-- Today execlaw is single-controller by design (per the 2026-04-23
-- locked decisions). The schema is forward-compatible with Phase-7
-- multi-controller / role-scoped operators — the `role` column
-- already exists; today's only valid value is "controller".
--
-- `user_id` is the same string as the corresponding `principals.id`
-- row, so the operator's auth identity and their participant identity
-- are unified.
CREATE TABLE IF NOT EXISTS users (
    user_id        TEXT    PRIMARY KEY,
    display_name   TEXT    NOT NULL,
    email          TEXT,
    password_hash  TEXT    NOT NULL,        -- Argon2id (§7.1)
    role           TEXT    NOT NULL,        -- "controller" | "operator" | "viewer" (Phase 7+)
    created_at     INTEGER NOT NULL,
    last_login_at  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_users_role ON users(role);
