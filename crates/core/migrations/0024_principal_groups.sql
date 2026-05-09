-- 0024 — Principal groups (Phase 16: per-group container runners).
--
-- A principal group is the unit of execution for a runner container.
-- Identity: `(channel, native_group_id)` when the channel exposes a
-- group id (Signal/WhatsApp/Slack do), otherwise `(channel, sorted-
-- principal-set-hash)` for ad-hoc DMs and the web SPA.
--
-- One runner container is spawned per group. Multiple `state_conversations`
-- rows (display threads) can map to the same group — the controller's
-- web SPA threads all share `(web, {controller})`.

CREATE TABLE IF NOT EXISTS state_principal_groups (
    -- Internal uuid we mint. Used by the runner registry, the
    -- workspace volume name (`execlaw-runner-<group_id>`), and the
    -- foreign key from state_conversations.principal_group_id.
    group_id            TEXT PRIMARY KEY,

    -- Transport identifier: 'web' | 'whatsapp' | 'signal' | 'slack' |
    -- 'email' | ... Free-form because the channel set is plugin-driven
    -- (transport plugins land in Phase 8). The runner doesn't care
    -- about the value beyond using it as part of the lookup key.
    channel             TEXT NOT NULL,

    -- Channel's native group id when one exists (e.g. WhatsApp's
    -- group jid). NULL for transports that don't have group ids
    -- (web, ad-hoc DMs). When present, it's the authoritative
    -- identity component for `(channel, native_group_id)` lookups.
    native_group_id     TEXT,

    -- sha256 hex of the sorted principal_id set. Used as a fallback
    -- identity when `native_group_id` is NULL — two messages from
    -- the same set of principals on the same channel hash to the
    -- same group. Kept on every row (even when native_group_id is
    -- present) so the membership-driven UNIQUE constraint stays
    -- meaningful for transports that mix.
    principal_set_hash  TEXT NOT NULL,

    -- Denormalised flag for the reaper: groups containing the
    -- controller are pinned hot indefinitely (never idle-reaped,
    -- workspace never wiped). Recomputed from
    -- state_principal_group_members on every membership change.
    includes_controller INTEGER NOT NULL DEFAULT 0,

    created_at          INTEGER NOT NULL,
    last_active_at      INTEGER NOT NULL
);

-- Channel-native id wins when present. Two groups on the same
-- channel can't share a native_group_id.
CREATE UNIQUE INDEX IF NOT EXISTS idx_principal_groups_channel_native
    ON state_principal_groups(channel, native_group_id)
    WHERE native_group_id IS NOT NULL;

-- Membership-set fallback. Two groups on the same channel can't have
-- the same principal set when no native_group_id distinguishes them.
CREATE UNIQUE INDEX IF NOT EXISTS idx_principal_groups_channel_hash
    ON state_principal_groups(channel, principal_set_hash)
    WHERE native_group_id IS NULL;

CREATE TABLE IF NOT EXISTS state_principal_group_members (
    group_id     TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    PRIMARY KEY(group_id, principal_id)
);

CREATE INDEX IF NOT EXISTS idx_pg_members_principal
    ON state_principal_group_members(principal_id);

-- Bridge from display thread → execution unit. Nullable so the
-- migration is non-destructive — older threads can be lazily
-- back-filled the next time they receive a turn (resolve_principal_group
-- writes the column on first sighting).
ALTER TABLE state_conversations ADD COLUMN principal_group_id TEXT;

CREATE INDEX IF NOT EXISTS idx_state_conversations_pg
    ON state_conversations(principal_group_id);
