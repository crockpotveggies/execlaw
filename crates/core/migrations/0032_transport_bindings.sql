-- 0032 — Transport bindings (Bridge supervisor Phase 2a).
--
-- Flat, indexed mapping from a transport's foreign id (Signal JID,
-- WhatsApp WAID, Matrix MXID, etc.) to the execlaw `principal_group_id`
-- that owns the conversation with that endpoint.
--
-- Why a separate table when `principals.identifiers_json` already
-- stores `(transport, handle)` pairs?
--   1. Inbound routing is on the hot path — every inbound message
--      needs `O(1)` lookup from `(channel, foreign_id)` →
--      principal_group_id. Scanning a JSON blob across N principals
--      per inbound is unacceptable; a btree index on this table is
--      sub-µs.
--   2. Group bindings (Signal group ids, WhatsApp group jids) don't
--      live on individual principals — they're properties of the
--      principal_group itself. `state_principal_groups.native_group_id`
--      already covers the group case structurally; this table
--      unifies group + DM lookup behind a single key shape so the
--      bridge supervisor doesn't branch on shape per transport.
--   3. Outbound dispatch needs the inverse: principal_group_id →
--      foreign_id. Same table, opposite index.

CREATE TABLE IF NOT EXISTS state_transport_bindings (
    -- Transport identifier — same string used in
    -- state_principal_groups.channel: 'signal', 'whatsapp', 'web', ...
    channel             TEXT NOT NULL,

    -- Transport's foreign id for this endpoint. For Signal: the JID
    -- (`signal:user:<uuid>` for DMs, `signal:group:<id>` for groups).
    -- For WhatsApp: the wa.me handle. For email: the canonical
    -- mailbox. Bridge plugins decide the format; the supervisor and
    -- the store just key on the literal string.
    foreign_id          TEXT NOT NULL,

    -- The execlaw principal_group_id this binding routes to.
    -- Required, NOT NULL: a binding without a target is dead weight.
    -- We don't FK because `state_principal_groups` doesn't have a
    -- foreign-key-friendly trigger story for cascade-on-delete in
    -- SQLite without WAL+foreign_keys=ON; the supervisor's delete
    -- path explicitly removes bindings when a group is destroyed
    -- (see `TransportBindingStore::forget_principal_group`).
    principal_group_id  TEXT NOT NULL,

    -- 0 = DM (foreign_id is one contact); 1 = group (foreign_id is
    -- a transport-side group id). Stored explicitly even though
    -- it's recoverable from state_principal_groups so inbound
    -- routing doesn't need the join.
    is_group            INTEGER NOT NULL DEFAULT 0,

    -- When the supervisor first observed this mapping. Useful for
    -- diagnostics ("when did we start routing this contact's
    -- messages here?") and for retention sweeps.
    created_at          INTEGER NOT NULL,

    -- Last inbound or outbound activity on this binding. The
    -- supervisor updates this on every dispatch and ingest;
    -- retention can prune bindings that haven't seen activity in N
    -- months without losing identity continuity (the principal
    -- record itself is the durable identity — bindings are the hot
    -- index).
    last_seen_at        INTEGER,

    -- (channel, foreign_id) is the natural primary key — a single
    -- foreign endpoint can only point at one principal_group at a
    -- time per transport. Re-pointing is a deliberate operator
    -- action via `upsert_binding`.
    PRIMARY KEY (channel, foreign_id)
);

-- Outbound: principal_group_id → foreign_id. Same row, opposite
-- index. Keeping it separate from the PK lets the supervisor
-- enumerate "all foreign ids for this group" cheaply (Signal could
-- legitimately have one DM JID + one group JID for the same
-- principal_group if the group includes a single contact).
CREATE INDEX IF NOT EXISTS idx_transport_bindings_pg
    ON state_transport_bindings(principal_group_id);

-- Retention sweep + dashboard queries ("show me Signal bindings idle
-- for 90 days") want a (channel, last_seen_at) scan; this index
-- keeps that off a full table scan once the table grows past a few
-- hundred bindings.
CREATE INDEX IF NOT EXISTS idx_transport_bindings_channel_last_seen
    ON state_transport_bindings(channel, last_seen_at);
