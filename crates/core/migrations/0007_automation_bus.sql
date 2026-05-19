-- 2026-05-17 — M1 of Automations: durable event bus substrate.
--
-- `state_bus_events` is a SEPARATE table from `state_events` (which is
-- the per-conversation append-only event log). The bus carries
-- *external* signals — webhooks, sockets, plugin emits, routine
-- completions — that automations subscribe to. No conversation
-- identity, no HMAC chain, no replay-for-state-reconstruction
-- semantics. The two tables share no foreign keys and no rows; they
-- are independent substrates with different invariants.
--
-- Dedup contract: `id` is the primary key, supplied by the producer.
-- Producers that want dedup semantics (across upstream retries,
-- socket reconnect-replay, plugin at-least-once delivery) supply a
-- stable ID (content hash, upstream message ID); producers that
-- don't care supply a random ULID. The bus has no opinion; PK
-- uniqueness is the entire dedup story. Duplicate inserts return
-- the existing row without error (see `BusEventStore::publish`).
--
-- `internal = 1` marks events produced by in-process consumers (flow
-- side effects, plugin emits) which write directly to SQLite and
-- are picked up by a polling task. Ingress events (webhooks, etc.)
-- ride the bounded `tokio::sync::mpsc` channel in front of the
-- dispatcher and have `internal = 0`. The split avoids any chance
-- of producer-consumer deadlock through the channel.
--
-- `dispatched_at` records when the dispatcher has fanned the event
-- to matching automation runs. NULL = pending (the dispatcher
-- re-enqueues these on process restart). The partial index serves
-- the crash-recovery query specifically.
--
-- Retention: subject to the global `history_retention_days` policy
-- like every other history table. See `crates/server/src/research/
-- bus_event_retention.rs` for the sweeper.

CREATE TABLE state_bus_events (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    source        TEXT NOT NULL,
    received_at   INTEGER NOT NULL,
    payload       TEXT NOT NULL,
    internal      INTEGER NOT NULL DEFAULT 0,
    dispatched_at INTEGER
);

-- Lookup by kind for the dispatcher's matching pass.
CREATE INDEX idx_bus_events_kind_received
    ON state_bus_events(kind, received_at);

-- Partial index over rows the dispatcher still owes work for. The
-- predicate keeps the index narrow — only the (likely small) tail of
-- undispatched rows lives here, so the crash-recovery scan and the
-- internal poller's tick both stay cheap regardless of total history.
CREATE INDEX idx_bus_events_pending
    ON state_bus_events(internal, received_at)
    WHERE dispatched_at IS NULL;
