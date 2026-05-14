-- Companion index table for `principals.identifiers_json` so
-- `PrincipalStore::find_by_identifier` is an O(1) PK lookup instead of
-- an O(N) scan over every principal.
--
-- Why we need this NOW: every external inbound (Signal, WhatsApp,
-- future bridges) hits `admit_external_principal::find_by_identifier`
-- to check whether the sender is already a known principal. Without
-- the index, an install with 500+ contacts pays a per-inbound full-
-- table read where the principal row is the answer to a (transport,
-- handle) tuple.
--
-- The original baseline schema flagged this gap in a comment at
-- `crates/core/src/principal.rs:218` — *"Phase 3 implementation:
-- scan rows and check membership. When the principal count grows,
-- a companion index table lands."* This is that landing.
--
-- Composite PK on (transport, handle, principal_id) — NOT (transport,
-- handle) — because a single identifier can be claimed by multiple
-- principals during reconcile windows: a stale UnknownPending row
-- shadowing a controller-asserted "My identities" mapping is the
-- canonical case (see `principal_admit::reconcile_against_my_identities`).
-- The reconcile path needs `find_all_by_identifier` to enumerate ALL
-- claimants; allowing multiple rows for the same (transport, handle)
-- preserves that contract.
--
-- Backfill: walk `principals.identifiers_json` and explode each
-- identifier into its own row. Uses `json_each` (SQLite 3.38+, well
-- inside our minimum-supported version range).
--
-- The `ON DELETE CASCADE` keeps the index honest: drop a principal
-- and its identifiers vanish too. Together with the upsert path's
-- "delete-then-reinsert" semantics in PrincipalStore, this means
-- the index can never drift from `identifiers_json` for any
-- principal still in the table.

CREATE TABLE principal_identifiers (
    transport      TEXT    NOT NULL,
    handle         TEXT    NOT NULL,
    principal_id   TEXT    NOT NULL,
    -- Refresh on every observation. Distinct from `principals.last_seen`
    -- because a principal can be seen on multiple identifiers and the
    -- per-(transport, handle) recency is the useful one for "the
    -- contact's Signal handle is alive even though their WhatsApp went
    -- quiet" surfaces.
    last_seen      INTEGER,
    PRIMARY KEY (transport, handle, principal_id),
    FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE
);

-- Composite lookup index — the most common query is
-- `WHERE transport = ? AND handle = ?` (find_by_identifier).
-- The PK already covers this prefix, so SQLite's automatic PK index
-- serves the query; this explicit index is documentation + insurance
-- against future PK reordering.
CREATE INDEX idx_principal_identifiers_lookup
    ON principal_identifiers(transport, handle);

-- Reverse lookup for upsert / cascade-delete and for the future
-- "list every identifier this principal claims" admin surface.
CREATE INDEX idx_principal_identifiers_by_pid
    ON principal_identifiers(principal_id);

-- Backfill from existing `principals.identifiers_json`. The JSON
-- shape is `[{"transport": "...", "handle": "..."}, ...]` — we
-- explode each array element into a row. `INSERT OR IGNORE` so
-- a re-run (e.g. after a manual edit) is a no-op rather than a
-- PK-conflict failure.
-- `identifiers_json` is declared BLOB (serde_json::to_vec output),
-- so we CAST AS TEXT to give `json_each` a text argument it can
-- parse. SQLite's JSON1 doesn't auto-decode BLOB-typed JSON.
INSERT OR IGNORE INTO principal_identifiers(transport, handle, principal_id, last_seen)
SELECT
    json_extract(ident.value, '$.transport')   AS transport,
    json_extract(ident.value, '$.handle')      AS handle,
    p.id                                        AS principal_id,
    p.last_seen                                 AS last_seen
FROM principals p, json_each(CAST(p.identifiers_json AS TEXT)) AS ident
WHERE json_extract(ident.value, '$.transport') IS NOT NULL
  AND json_extract(ident.value, '$.handle')    IS NOT NULL;
