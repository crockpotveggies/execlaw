//! Transport-binding store (Bridge supervisor Phase 2a).
//!
//! Indexed mapping from `(channel, foreign_id)` to
//! `principal_group_id`. The bridge supervisor consults this on every
//! inbound message ("which conversation owns this Signal JID?") and
//! every outbound dispatch ("what's the foreign id for this group?").
//! Hot path — every read goes through one indexed SQLite query, no
//! JSON parsing.
//!
//! See `migrations/0032_transport_bindings.sql` for the schema and
//! the rationale for keeping this separate from
//! `principals.identifiers_json`.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// One row from `state_transport_bindings`. Cheap to clone — every
/// field is a small string or scalar. `Serialize`/`Deserialize` so
/// the supervisor can ship these into events / HTTP responses
/// without a separate DTO layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportBinding {
    pub channel: String,
    pub foreign_id: String,
    pub principal_group_id: String,
    pub is_group: bool,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

/// Storage façade. Holds only `&Database`, so cloning the wrapper is
/// free and tests can construct one per case without a connection
/// pool.
pub struct TransportBindingStore<'db> {
    db: &'db Database,
}

impl<'db> TransportBindingStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a fresh binding. On `(channel, foreign_id)` conflict,
    /// the existing row is left untouched and the call returns
    /// `Ok(false)` — the supervisor uses that as a "we already
    /// route this endpoint somewhere; pick `repoint_binding` if you
    /// really mean to move it" signal. Successful insert returns
    /// `Ok(true)`.
    ///
    /// Why split insert/repoint?  An earlier draft of this API was
    /// a single `upsert_binding` that silently rebound the
    /// `principal_group_id` on conflict. That made first-insert and
    /// "steal an existing binding" syntactically identical — a
    /// buggy bridge that fabricated a different `principal_group_id`
    /// on a re-ingest could silently steal a contact's binding away
    /// from the right group with no audit trace. Forcing the rebind
    /// to be a separate, named call makes the steal show up in
    /// grep + lets us return the displaced pg for the audit log.
    ///
    /// `is_group` is denormalised into the row at insert time. On
    /// the no-op conflict path nothing is updated, so a flipped
    /// `is_group` argument is silently ignored (matches the
    /// "existing row left untouched" contract).
    pub fn insert_binding(
        &self,
        channel: &str,
        foreign_id: &str,
        principal_group_id: &str,
        is_group: bool,
        now: i64,
    ) -> Result<bool, DbError> {
        let is_group_i: i64 = if is_group { 1 } else { 0 };
        self.db.with_conn(|c| {
            let n = c.execute(
                "INSERT INTO state_transport_bindings \
                 (channel, foreign_id, principal_group_id, is_group, created_at, last_seen_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT(channel, foreign_id) DO NOTHING",
                params![channel, foreign_id, principal_group_id, is_group_i, now],
            )?;
            Ok(n > 0)
        })
    }

    /// Re-point an existing binding at a different
    /// `principal_group_id`. Returns `Ok(Some(old_pg))` with the
    /// displaced group (which the supervisor logs to the audit
    /// trail), or `Ok(None)` if no binding existed in the first
    /// place. Bumps `last_seen_at` to `now` because rebinding is
    /// activity.
    ///
    /// Does NOT touch `is_group` — see the doc-comment on
    /// `insert_binding` for why a binding's DM/group nature is
    /// considered structural and shouldn't quietly flip on a rebind.
    /// If a transport genuinely needs to re-classify a binding,
    /// that's a `delete_binding` + `insert_binding` pair, not a
    /// repoint.
    pub fn repoint_binding(
        &self,
        channel: &str,
        foreign_id: &str,
        new_principal_group_id: &str,
        now: i64,
    ) -> Result<Option<String>, DbError> {
        self.db.with_conn(|c| {
            // Phase 1: read the current pg so we can return it.
            let old = match c.query_row(
                "SELECT principal_group_id FROM state_transport_bindings \
                 WHERE channel = ?1 AND foreign_id = ?2",
                params![channel, foreign_id],
                |r| r.get::<_, String>(0),
            ) {
                Ok(pg) => pg,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            // Phase 2: rebind. We could skip this when
            // `old == new_principal_group_id` to avoid touching
            // `last_seen_at` for a no-op rebind, but the explicit
            // `touch_binding` path is the right hook for that — a
            // rebind to the same group is still operator intent
            // worth recording.
            c.execute(
                "UPDATE state_transport_bindings \
                 SET principal_group_id = ?3, last_seen_at = ?4 \
                 WHERE channel = ?1 AND foreign_id = ?2",
                params![channel, foreign_id, new_principal_group_id, now],
            )?;
            Ok(Some(old))
        })
    }

    /// **Hot path**: inbound routing. Returns the
    /// `principal_group_id` that owns this `(channel, foreign_id)`,
    /// or `None` if the bridge supervisor has never seen this
    /// endpoint. A miss tells the supervisor to kick the first-
    /// contact / silent-hold flow before creating a conversation.
    ///
    /// Does NOT update `last_seen_at` — the caller decides whether
    /// the lookup is followed by activity worth recording. (A
    /// retention sweep that probes existence shouldn't reset the
    /// idle timer.) Use `touch_binding` after a real ingest.
    pub fn lookup_principal_group(
        &self,
        channel: &str,
        foreign_id: &str,
    ) -> Result<Option<String>, DbError> {
        self.db.with_conn(|c| {
            // Distinguish "no row" from a real DB fault. Pre-fix
            // this used `.ok()` which collapsed both into `None`,
            // letting a locked-database / corrupt-row failure look
            // identical to a first-contact miss — the supervisor
            // would silently kick the intake flow on what was
            // really a transient I/O fault, and the operator would
            // see no alert. Use SQLite's "no rows" sentinel
            // explicitly.
            match c.query_row(
                "SELECT principal_group_id FROM state_transport_bindings \
                 WHERE channel = ?1 AND foreign_id = ?2",
                params![channel, foreign_id],
                |r| r.get::<_, String>(0),
            ) {
                Ok(pg) => Ok(Some(pg)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    /// Outbound: list every foreign id this group has across the
    /// requested channel. Most groups have exactly one binding per
    /// channel, but Signal could legitimately bind both a group JID
    /// (`signal:group:<id>`) AND a per-member contact JID
    /// (`signal:user:<uuid>`) to the same `principal_group_id` —
    /// returning a list keeps the API honest about that.
    pub fn bindings_for_group(
        &self,
        principal_group_id: &str,
        channel: &str,
    ) -> Result<Vec<TransportBinding>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT channel, foreign_id, principal_group_id, is_group, \
                        created_at, last_seen_at \
                 FROM state_transport_bindings \
                 WHERE principal_group_id = ?1 AND channel = ?2 \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt
                .query_map(params![principal_group_id, channel], |r| {
                    Ok(TransportBinding {
                        channel: r.get(0)?,
                        foreign_id: r.get(1)?,
                        principal_group_id: r.get(2)?,
                        is_group: r.get::<_, i64>(3)? != 0,
                        created_at: r.get(4)?,
                        last_seen_at: r.get::<_, Option<i64>>(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Channel-agnostic variant of [`bindings_for_group`] — every
    /// transport that bound onto this principal_group, regardless
    /// of channel. Used by `principal_admit::reconcile` which
    /// needs to repoint every binding pointing at a stale group
    /// without enumerating channels itself.
    pub fn bindings_for_group_any_channel(
        &self,
        principal_group_id: &str,
    ) -> Result<Vec<TransportBinding>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT channel, foreign_id, principal_group_id, is_group, \
                        created_at, last_seen_at \
                 FROM state_transport_bindings \
                 WHERE principal_group_id = ?1 \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt
                .query_map(params![principal_group_id], |r| {
                    Ok(TransportBinding {
                        channel: r.get(0)?,
                        foreign_id: r.get(1)?,
                        principal_group_id: r.get(2)?,
                        is_group: r.get::<_, i64>(3)? != 0,
                        created_at: r.get(4)?,
                        last_seen_at: r.get::<_, Option<i64>>(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Bump `last_seen_at` for one binding without changing anything
    /// else. Called after a successful inbound ingest or outbound
    /// dispatch — tells retention this binding is still hot.
    /// Returns the number of rows updated (0 if the binding doesn't
    /// exist; the supervisor uses that as a no-op signal rather
    /// than an error).
    pub fn touch_binding(
        &self,
        channel: &str,
        foreign_id: &str,
        now: i64,
    ) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_transport_bindings \
                 SET last_seen_at = ?3 \
                 WHERE channel = ?1 AND foreign_id = ?2",
                params![channel, foreign_id, now],
            )?;
            Ok(n)
        })
    }

    /// Cascade-on-group-delete. The supervisor calls this when a
    /// `principal_group` is destroyed — the bridge has no
    /// foreign-key trigger because SQLite's foreign_keys pragma
    /// isn't reliably on across our codebase.
    /// Returns the number of bindings actually removed (useful for
    /// the supervisor's audit log).
    pub fn forget_principal_group(&self, principal_group_id: &str) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_transport_bindings \
                 WHERE principal_group_id = ?1",
                params![principal_group_id],
            )?;
            Ok(n)
        })
    }

    /// Single-binding delete by natural key. Used when an operator
    /// manually unbinds a contact from a group via the admin UI.
    /// Returns true iff a row was actually removed.
    pub fn delete_binding(&self, channel: &str, foreign_id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_transport_bindings \
                 WHERE channel = ?1 AND foreign_id = ?2",
                params![channel, foreign_id],
            )?;
            Ok(n > 0)
        })
    }

    /// Retention helper: list bindings on `channel` whose last
    /// activity was strictly before `cutoff`. The retention sweeper
    /// enumerates these and deletes them in a separate pass; we
    /// don't combine the SELECT and DELETE because the supervisor
    /// wants to log each removal.
    ///
    /// `cutoff` is a unix timestamp; the comparison is strict-less-
    /// than. Bindings with `last_seen_at IS NULL` are considered
    /// "never seen" and are returned only if `created_at < cutoff`,
    /// so a freshly-inserted binding doesn't get reaped before the
    /// supervisor has even had a chance to touch it. The strict
    /// boundary matches the colloquial reading of "idle since cutoff"
    /// — a binding touched precisely at `cutoff` is on-the-line, not
    /// idle.
    pub fn list_idle_bindings(
        &self,
        channel: &str,
        cutoff: i64,
    ) -> Result<Vec<TransportBinding>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT channel, foreign_id, principal_group_id, is_group, \
                        created_at, last_seen_at \
                 FROM state_transport_bindings \
                 WHERE channel = ?1 \
                   AND COALESCE(last_seen_at, created_at) < ?2 \
                 ORDER BY COALESCE(last_seen_at, created_at) ASC",
            )?;
            let rows = stmt
                .query_map(params![channel, cutoff], |r| {
                    Ok(TransportBinding {
                        channel: r.get(0)?,
                        foreign_id: r.get(1)?,
                        principal_group_id: r.get(2)?,
                        is_group: r.get::<_, i64>(3)? != 0,
                        created_at: r.get(4)?,
                        last_seen_at: r.get::<_, Option<i64>>(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn insert_then_lookup_round_trips_for_dm() {
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        let inserted = store
            .insert_binding("signal", "signal:user:abc", "pg-1", false, 1_000)
            .unwrap();
        assert!(inserted, "fresh insert should report true");
        let pg = store
            .lookup_principal_group("signal", "signal:user:abc")
            .unwrap()
            .expect("binding should exist");
        assert_eq!(pg, "pg-1");
    }

    #[test]
    fn insert_returns_false_on_conflict_and_does_not_steal() {
        // The whole point of splitting insert/repoint: a buggy bridge
        // calling insert_binding twice with different
        // principal_group_id values must NOT silently take the
        // binding away from the original group. The first call wins;
        // the second returns Ok(false) with the row untouched.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        let first = store
            .insert_binding("signal", "j", "pg-1", false, 1_000)
            .unwrap();
        let second = store
            .insert_binding("signal", "j", "pg-evil", false, 2_000)
            .unwrap();
        assert!(first);
        assert!(!second, "conflict path must report false");
        let pg = store.lookup_principal_group("signal", "j").unwrap();
        assert_eq!(pg.as_deref(), Some("pg-1"));
        // last_seen_at must NOT have been bumped — a no-op insert
        // shouldn't refresh retention timers.
        let row = store.bindings_for_group("pg-1", "signal").unwrap();
        assert_eq!(row[0].last_seen_at, Some(1_000));
    }

    #[test]
    fn insert_does_not_overwrite_is_group_on_conflict() {
        // Pin the doc-comment contract: a flipped is_group on a
        // conflict path must be silently ignored. (The whole row is
        // left untouched, which is the strongest version of this
        // invariant, but pin it so a future refactor that switches
        // to ON CONFLICT DO UPDATE doesn't accidentally start
        // mutating the flag.)
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "j", "pg", true, 1_000) // group
            .unwrap();
        store
            .insert_binding("signal", "j", "pg", false, 2_000) // flipped
            .unwrap();
        let row = store.bindings_for_group("pg", "signal").unwrap();
        assert!(
            row[0].is_group,
            "is_group must NOT flip on conflict — got {:?}",
            row,
        );
    }

    #[test]
    fn insert_preserves_created_at_on_conflict() {
        // A no-op insert must NOT mutate created_at — retention
        // sweeps anchor on this timestamp for never-touched
        // bindings.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "j", "pg", false, 1_000)
            .unwrap();
        store
            .insert_binding("signal", "j", "pg", false, 9_999)
            .unwrap();
        let row = store.bindings_for_group("pg", "signal").unwrap();
        assert_eq!(row[0].created_at, 1_000);
    }

    #[test]
    fn lookup_returns_none_for_unknown_endpoint() {
        // Inbound miss is the supervisor's signal to kick the
        // first-contact flow — if this collapsed to a phantom hit we
        // would silently route to the wrong group.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        assert!(
            store
                .lookup_principal_group("signal", "signal:user:never-seen")
                .unwrap()
                .is_none(),
        );
    }

    #[test]
    fn lookup_is_per_channel_so_signal_and_whatsapp_dont_collide() {
        // Defensive: a transport author who reuses the same string
        // shape for both protocols (unlikely but possible) shouldn't
        // see crossed wires. The PK is (channel, foreign_id) — pin it.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "+15551234567", "pg-signal", false, 1)
            .unwrap();
        store
            .insert_binding("whatsapp", "+15551234567", "pg-whatsapp", false, 2)
            .unwrap();
        assert_eq!(
            store
                .lookup_principal_group("signal", "+15551234567")
                .unwrap()
                .as_deref(),
            Some("pg-signal"),
        );
        assert_eq!(
            store
                .lookup_principal_group("whatsapp", "+15551234567")
                .unwrap()
                .as_deref(),
            Some("pg-whatsapp"),
        );
    }

    #[test]
    fn repoint_returns_old_pg_and_rebinds_atomically() {
        // The "this contact moved to a new group" flow — explicit,
        // returns the displaced group for the audit log.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "signal:user:abc", "pg-1", false, 1_000)
            .unwrap();
        let displaced = store
            .repoint_binding("signal", "signal:user:abc", "pg-2", 2_000)
            .unwrap();
        assert_eq!(displaced.as_deref(), Some("pg-1"));
        let bindings = store.bindings_for_group("pg-2", "signal").unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].principal_group_id, "pg-2");
        assert_eq!(bindings[0].last_seen_at, Some(2_000));
        // The old group should have nothing pointing at it now —
        // repointing is exclusive on (channel, foreign_id).
        let old = store.bindings_for_group("pg-1", "signal").unwrap();
        assert!(old.is_empty(), "old binding should be gone, got {old:?}");
    }

    #[test]
    fn repoint_returns_none_when_no_binding_exists() {
        // The supervisor uses None to distinguish "we just rebound
        // an existing binding" (worth audit-logging) from "there
        // was nothing here" (nothing to log).
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        let displaced = store.repoint_binding("signal", "ghost", "pg-x", 1).unwrap();
        assert!(displaced.is_none());
    }

    #[test]
    fn touch_binding_updates_last_seen_only() {
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "j", "pg", false, 1_000)
            .unwrap();
        let n = store.touch_binding("signal", "j", 5_000).unwrap();
        assert_eq!(n, 1);
        let bindings = store.bindings_for_group("pg", "signal").unwrap();
        assert_eq!(bindings[0].last_seen_at, Some(5_000));
        assert_eq!(bindings[0].created_at, 1_000); // unchanged
    }

    #[test]
    fn touch_binding_returns_zero_for_unknown_endpoint() {
        // No-op signal, not an error. The supervisor uses this to
        // skip the second SELECT after a successful dispatch when
        // there's nothing to touch (race with a concurrent unbind).
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        let n = store.touch_binding("signal", "never-seen", 1).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn forget_principal_group_cascades_all_its_bindings() {
        // Group delete must wipe every binding pointing at it; if
        // this misses one the supervisor would route an inbound
        // message to a now-gone group_id and the runner spawn would
        // fail mysteriously downstream.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "signal:user:a", "pg-1", false, 1)
            .unwrap();
        store
            .insert_binding("signal", "signal:user:b", "pg-1", false, 2)
            .unwrap();
        store
            .insert_binding("signal", "signal:group:gg", "pg-1", true, 3)
            .unwrap();
        // Sibling row that must NOT be touched.
        store
            .insert_binding("signal", "signal:user:c", "pg-2", false, 4)
            .unwrap();

        let removed = store.forget_principal_group("pg-1").unwrap();
        assert_eq!(removed, 3);
        assert!(
            store
                .lookup_principal_group("signal", "signal:user:a")
                .unwrap()
                .is_none(),
        );
        // Sibling survives.
        assert_eq!(
            store
                .lookup_principal_group("signal", "signal:user:c")
                .unwrap()
                .as_deref(),
            Some("pg-2"),
        );
    }

    #[test]
    fn forget_principal_group_returns_zero_for_unknown_pg() {
        // Counterpart to `touch_binding_returns_zero_for_unknown_endpoint`.
        // The supervisor's group-delete path tolerates "nothing to
        // forget" (e.g. an aborted spawn that never registered a
        // binding) without bubbling an error.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        let removed = store.forget_principal_group("pg-never-existed").unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn binding_with_is_group_true_round_trips() {
        // All other tests use is_group=false. Pin the bool decode
        // explicitly so a future "INTEGER → BOOLEAN" schema tweak
        // can't quietly break group detection.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "signal:group:abc", "pg", true, 1)
            .unwrap();
        let row = &store.bindings_for_group("pg", "signal").unwrap()[0];
        assert!(row.is_group);
    }

    #[test]
    fn delete_binding_returns_true_only_when_a_row_was_removed() {
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store.insert_binding("signal", "x", "pg", false, 1).unwrap();
        assert!(store.delete_binding("signal", "x").unwrap());
        // Second delete: nothing to remove.
        assert!(!store.delete_binding("signal", "x").unwrap());
    }

    #[test]
    fn bindings_for_group_orders_by_created_at_ascending() {
        // Stability — the supervisor's "list every binding for this
        // group" UI should render in a predictable order. ASC by
        // created_at is the natural choice (oldest binding first =
        // the one the operator probably remembers).
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "first", "pg", false, 1)
            .unwrap();
        store
            .insert_binding("signal", "second", "pg", false, 2)
            .unwrap();
        store
            .insert_binding("signal", "third", "pg", false, 3)
            .unwrap();
        let rows = store.bindings_for_group("pg", "signal").unwrap();
        assert_eq!(
            rows.iter()
                .map(|b| b.foreign_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"],
        );
    }

    #[test]
    fn list_idle_bindings_uses_created_at_when_never_touched() {
        // Edge case: a binding inserted but never `touch_binding`'d
        // (because the supervisor crashed between insert and the
        // first ingest). It should be reapable based on created_at
        // alone, not stuck forever because last_seen_at is NULL.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "old", "pg-1", false, 100)
            .unwrap();
        store
            .insert_binding("signal", "fresh", "pg-2", false, 5_000)
            .unwrap();
        // insert_binding sets last_seen_at = now. Force NULL to
        // exercise the COALESCE branch directly.
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_transport_bindings SET last_seen_at = NULL \
                 WHERE foreign_id IN ('old', 'fresh')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        // cutoff = 1_000 → 'old' (created_at=100) is strict-less-than,
        // 'fresh' (created_at=5_000) is not.
        let idle = store.list_idle_bindings("signal", 1_000).unwrap();
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].foreign_id, "old");
    }

    #[test]
    fn list_idle_bindings_respects_last_seen_when_present() {
        // The common case: binding touched recently, retention
        // shouldn't pick it up.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "fresh", "pg", false, 100)
            .unwrap();
        store.touch_binding("signal", "fresh", 9_000).unwrap();
        let idle = store.list_idle_bindings("signal", 5_000).unwrap();
        assert!(idle.is_empty(), "fresh binding should not be idle");
    }

    #[test]
    fn list_idle_bindings_excludes_boundary_match() {
        // Strict-less-than semantics — a binding touched precisely
        // at `cutoff` is on-the-line, not idle. Pre-fix the code
        // used `<=` which would have reaped this row.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding("signal", "edge", "pg", false, 100)
            .unwrap();
        store.touch_binding("signal", "edge", 5_000).unwrap();
        let idle = store.list_idle_bindings("signal", 5_000).unwrap();
        assert!(
            idle.is_empty(),
            "binding touched at exactly cutoff must NOT be listed",
        );
    }

    #[test]
    fn list_idle_bindings_with_zero_cutoff_returns_empty() {
        // Boundary check: cutoff = 0 → nothing predates it (assuming
        // unix timestamps), so the sweep is a no-op.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store.insert_binding("signal", "x", "pg", false, 1).unwrap();
        let idle = store.list_idle_bindings("signal", 0).unwrap();
        assert!(idle.is_empty());
    }

    #[test]
    fn list_idle_bindings_with_max_cutoff_returns_everything() {
        // Symmetric boundary: cutoff = i64::MAX → everything
        // touched/created strictly before MAX (everything) lists.
        // The supervisor would never actually do this, but the
        // contract should be honest.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store.insert_binding("signal", "a", "pg", false, 1).unwrap();
        store.insert_binding("signal", "b", "pg", false, 2).unwrap();
        let idle = store.list_idle_bindings("signal", i64::MAX).unwrap();
        assert_eq!(idle.len(), 2);
    }

    #[test]
    fn insert_succeeds_for_nonexistent_principal_group() {
        // Documents the deliberate absence of an FK to
        // state_principal_groups: bindings can name a pg that
        // doesn't exist yet (or never did). The supervisor's
        // first-contact flow relies on this — it inserts the
        // binding before resolving the actual principal_group row,
        // so the row order is binding-first, group-second within
        // the same transaction. A future maintainer "helpfully"
        // adding an FK would silently invert that order.
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        let ok = store
            .insert_binding("signal", "x", "pg-does-not-exist-anywhere", false, 1)
            .unwrap();
        assert!(ok);
    }
}
