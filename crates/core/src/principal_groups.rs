//! Principal-group identity (Phase 16).
//!
//! A principal group is the unit a runner container serves: a stable
//! `(channel, set-of-principals)` tuple. Multiple display threads
//! (rows in `state_conversations`) collapse onto one group when they
//! share the same channel + participants — most importantly, every
//! controller-initiated SPA thread shares `(web, {controller})` and
//! therefore one runner.
//!
//! Identity rules:
//!
//!   * **Channel-native group id wins** when the transport supplies
//!     one (WhatsApp's group jid, Signal's group id, Slack's channel
//!     id). Two messages with the same `(channel, native_group_id)`
//!     resolve to the same row regardless of which principals are
//!     currently in the membership list — the channel is the
//!     authority.
//!
//!   * **Sorted-principal-set hash** is the fallback when no native
//!     id is available (web SPA, ad-hoc DMs, transports that don't
//!     expose group identity). Hash is sha-256 over the sorted
//!     principal_id strings joined by `\n`, hex-encoded.
//!
//! Both indices live on the table; the SQL `WHERE native_group_id IS
//! NOT NULL` partial index lets us enforce uniqueness on the strong
//! identity without forcing every ad-hoc DM to invent a fake native
//! id.
//!
//! `includes_controller` is denormalised — recomputed from the
//! members table on every membership write — because the runner
//! reaper reads it on every sweep and we don't want to JOIN every
//! 60 seconds.

use crate::db::{Database, DbError};
use crate::ids::PrincipalId;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One row in `state_principal_groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalGroup {
    pub group_id: String,
    pub channel: String,
    pub native_group_id: Option<String>,
    pub principal_set_hash: String,
    pub includes_controller: bool,
    pub created_at: i64,
    pub last_active_at: i64,
}

/// Inputs to `resolve_principal_group`. The caller builds this from
/// whatever the inbound transport handed it. `principals` MUST
/// contain every participant in the conversation (including the
/// controller when present) so the hash matches future calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupKey<'a> {
    pub channel: &'a str,
    pub native_group_id: Option<&'a str>,
    pub principals: &'a [PrincipalId],
    /// True when the controller principal is in `principals`. The
    /// caller is responsible for setting this — `principal_id` alone
    /// doesn't tell us trust class.
    pub includes_controller: bool,
}

/// Compute the `principal_set_hash` for a sorted set of principal
/// ids. Stable across calls: sort, dedup, join with `\n`, sha-256,
/// hex.
pub fn principal_set_hash(principals: &[PrincipalId]) -> String {
    let mut ids: Vec<&str> = principals.iter().map(PrincipalId::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut hasher = Sha256::new();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\n");
        }
        hasher.update(id.as_bytes());
    }
    let bytes = hasher.finalize();
    hex::encode(bytes)
}

/// Mint a fresh group_id. UUID v4 — the column is the join key for
/// runner registry, workspace volume name, etc.
fn mint_group_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Storage façade for principal groups. Cheap to construct (holds
/// only a `&Database`).
pub struct PrincipalGroupStore<'db> {
    db: &'db Database,
}

impl<'db> PrincipalGroupStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Lookup-or-insert: resolve `(channel, native_group_id?,
    /// principals)` to a stable `PrincipalGroup`. Idempotent.
    ///
    /// Resolution order:
    ///   1. `(channel, native_group_id)` when `native_group_id` is
    ///      present — channel-native id wins.
    ///   2. `(channel, principal_set_hash)` otherwise.
    ///   3. Insert a fresh row when neither hits.
    ///
    /// Membership is reconciled to match `key.principals` on every
    /// call (a row can gain/lose members over time — e.g. someone
    /// joins a WhatsApp group). `includes_controller` is recomputed
    /// from the new membership AND `key.includes_controller` —
    /// either signal flipping it true sticks.
    ///
    /// Updates `last_active_at` to `now` so the supervisor's reaper
    /// has a fresh timestamp.
    pub fn resolve(&self, key: &GroupKey<'_>, now: i64) -> Result<PrincipalGroup, DbError> {
        let hash = principal_set_hash(key.principals);

        // Phase 1: lookup existing row.
        let existing: Option<(String, Option<String>, String, i64, i64)> =
            self.db.with_conn(|c| {
                if let Some(native) = key.native_group_id {
                    let row = c
                        .query_row(
                            "SELECT group_id, native_group_id, principal_set_hash, \
                                    created_at, last_active_at \
                             FROM state_principal_groups \
                             WHERE channel = ?1 AND native_group_id = ?2",
                            params![key.channel, native],
                            |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, Option<String>>(1)?,
                                    r.get::<_, String>(2)?,
                                    r.get::<_, i64>(3)?,
                                    r.get::<_, i64>(4)?,
                                ))
                            },
                        )
                        .ok();
                    return Ok(row);
                }
                let row = c
                    .query_row(
                        "SELECT group_id, native_group_id, principal_set_hash, \
                                created_at, last_active_at \
                         FROM state_principal_groups \
                         WHERE channel = ?1 AND principal_set_hash = ?2 \
                           AND native_group_id IS NULL",
                        params![key.channel, &hash],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, i64>(3)?,
                                r.get::<_, i64>(4)?,
                            ))
                        },
                    )
                    .ok();
                Ok(row)
            })?;

        let (group_id, created_at, _stored_hash) = match existing {
            Some((gid, _native, stored_hash, created, _last)) => (gid, created, stored_hash),
            None => {
                // Insert fresh.
                let gid = mint_group_id();
                let includes_controller_i: i64 = if key.includes_controller { 1 } else { 0 };
                self.db.with_conn(|c| {
                    c.execute(
                        "INSERT INTO state_principal_groups \
                         (group_id, channel, native_group_id, principal_set_hash, \
                          includes_controller, created_at, last_active_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            gid,
                            key.channel,
                            key.native_group_id,
                            &hash,
                            includes_controller_i,
                            now,
                            now,
                        ],
                    )?;
                    Ok(())
                })?;
                (gid, now, hash.clone())
            }
        };

        // Reconcile membership. We do a full diff because the
        // member set can shift between calls (someone added/removed
        // from a group). Cost is O(members) per resolve, which is
        // small enough for human-scale conversation sizes.
        self.reconcile_members(&group_id, key.principals, key.includes_controller, now)?;

        // Read back the full row (membership reconcile may have
        // flipped `includes_controller`).
        let row = self.db.with_conn(|c| {
            let row = c.query_row(
                "SELECT group_id, channel, native_group_id, principal_set_hash, \
                        includes_controller, created_at, last_active_at \
                 FROM state_principal_groups \
                 WHERE group_id = ?1",
                params![&group_id],
                |r| {
                    Ok(PrincipalGroup {
                        group_id: r.get(0)?,
                        channel: r.get(1)?,
                        native_group_id: r.get(2)?,
                        principal_set_hash: r.get(3)?,
                        includes_controller: r.get::<_, i64>(4)? != 0,
                        created_at: r.get(5)?,
                        last_active_at: r.get(6)?,
                    })
                },
            )?;
            Ok(row)
        })?;
        let _ = created_at;
        Ok(row)
    }

    /// Replace the membership rows for `group_id` with `principals`.
    /// Sets `includes_controller` to `true` when either the new
    /// membership flag is true OR the existing row already had it
    /// — once a group has had the controller in it, we treat it as
    /// controller-touched indefinitely. (Membership lists drift but
    /// trust history doesn't reset.)
    fn reconcile_members(
        &self,
        group_id: &str,
        principals: &[PrincipalId],
        includes_controller: bool,
        now: i64,
    ) -> Result<(), DbError> {
        // Dedupe and sort the input so the wipe-and-rewrite below
        // doesn't see duplicates.
        let mut seen: Vec<&str> = principals.iter().map(PrincipalId::as_str).collect();
        seen.sort_unstable();
        seen.dedup();

        self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM state_principal_group_members WHERE group_id = ?1",
                params![group_id],
            )?;
            for pid in &seen {
                tx.execute(
                    "INSERT INTO state_principal_group_members \
                     (group_id, principal_id) VALUES (?1, ?2)",
                    params![group_id, pid],
                )?;
            }
            // includes_controller is sticky-true (see doc).
            let new_flag: i64 = if includes_controller { 1 } else { 0 };
            tx.execute(
                "UPDATE state_principal_groups \
                 SET includes_controller = MAX(includes_controller, ?1), \
                     last_active_at = ?2 \
                 WHERE group_id = ?3",
                params![new_flag, now, group_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Lookup by id. Returns `None` when the row doesn't exist —
    /// e.g. after a delete.
    pub fn get(&self, group_id: &str) -> Result<Option<PrincipalGroup>, DbError> {
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT group_id, channel, native_group_id, principal_set_hash, \
                            includes_controller, created_at, last_active_at \
                     FROM state_principal_groups \
                     WHERE group_id = ?1",
                    params![group_id],
                    |r| {
                        Ok(PrincipalGroup {
                            group_id: r.get(0)?,
                            channel: r.get(1)?,
                            native_group_id: r.get(2)?,
                            principal_set_hash: r.get(3)?,
                            includes_controller: r.get::<_, i64>(4)? != 0,
                            created_at: r.get(5)?,
                            last_active_at: r.get(6)?,
                        })
                    },
                )
                .ok();
            Ok(row)
        })
    }

    /// Idempotent insert of a single `(group_id, principal_id)` row
    /// into `state_principal_group_members`. Unlike `resolve` (which
    /// reconciles via wipe-and-rewrite), this grows the membership
    /// without disturbing existing rows.
    ///
    /// The use case is "I observed one participant on the wire and
    /// want the host to know about them" — the per-message routing
    /// path in `generic_inbound::route_inbound` calls this for the
    /// sender + controller on every group inbound. Transports that
    /// don't expose the full group roster up-front (Signal,
    /// WhatsApp, Slack — none of their per-message webhooks include
    /// "here are all 8 participants") need this to grow the
    /// membership lazily; otherwise `members.len()` stays 0
    /// forever and `should_dispatch_to_agent`'s eligibility gate
    /// short-circuits to `EligibilityBypass` on every turn.
    ///
    /// Bumps `last_active_at` on the parent row so reaper bookkeeping
    /// stays current — the same way `resolve` does.
    pub fn add_member(
        &self,
        group_id: &str,
        principal_id: &PrincipalId,
        now: i64,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO state_principal_group_members \
                 (group_id, principal_id) VALUES (?1, ?2)",
                params![group_id, principal_id.as_str()],
            )?;
            tx.execute(
                "UPDATE state_principal_groups \
                 SET last_active_at = ?1 \
                 WHERE group_id = ?2",
                params![now, group_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// True iff `group_id` is the controller's solo DM runner —
    /// exactly one member AND `includes_controller=true`. The runner
    /// supervisor uses this to decide whether a runner is reap-exempt.
    ///
    /// A group that merely INCLUDES the controller (a Signal /
    /// WhatsApp / Slack group the operator's identity is one of N
    /// members of) is a regular group-context runner that follows
    /// the normal idle-reap lifecycle, NOT the always-warm controller
    /// runner that backs the SPA.
    ///
    /// Pre-fix the supervisor read `includes_controller` straight
    /// into `RunnerHandle.controller_runner`. That marked every
    /// Signal-group runner as un-reapable — shared group runners
    /// stayed up forever and the operator saw "N runners owned by
    /// the controller" in diagnostics.
    pub fn is_controller_solo(&self, group_id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT includes_controller, \
                            (SELECT COUNT(*) FROM state_principal_group_members \
                             WHERE group_id = ?1) AS member_count \
                     FROM state_principal_groups \
                     WHERE group_id = ?1",
                    params![group_id],
                    |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)?)),
                )
                .ok();
            Ok(matches!(row, Some((true, 1))))
        })
    }

    /// List every member principal_id of `group_id`. Empty when the
    /// group doesn't exist.
    pub fn members(&self, group_id: &str) -> Result<Vec<PrincipalId>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT principal_id FROM state_principal_group_members \
                 WHERE group_id = ?1 ORDER BY principal_id ASC",
            )?;
            let rows = stmt
                .query_map(params![group_id], |r| {
                    Ok(PrincipalId::from(r.get::<_, String>(0)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// All groups, ordered most-recently-active first. Used by the
    /// supervisor's boot sweep to enumerate which workspace volumes
    /// are still expected.
    pub fn list_all(&self) -> Result<Vec<PrincipalGroup>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT group_id, channel, native_group_id, principal_set_hash, \
                        includes_controller, created_at, last_active_at \
                 FROM state_principal_groups \
                 ORDER BY last_active_at DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(PrincipalGroup {
                        group_id: r.get(0)?,
                        channel: r.get(1)?,
                        native_group_id: r.get(2)?,
                        principal_set_hash: r.get(3)?,
                        includes_controller: r.get::<_, i64>(4)? != 0,
                        created_at: r.get(5)?,
                        last_active_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Hard-delete a group + its membership rows. Used when the
    /// operator explicitly drops a group (and the supervisor
    /// already wiped the workspace + container). Returns true when
    /// a row was actually removed.
    pub fn delete(&self, group_id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM state_principal_group_members WHERE group_id = ?1",
                params![group_id],
            )?;
            let n = tx.execute(
                "DELETE FROM state_principal_groups WHERE group_id = ?1",
                params![group_id],
            )?;
            tx.commit()?;
            Ok(n > 0)
        })
    }

    /// Stamp the `last_active_at` column. The supervisor calls this
    /// when a turn finishes so the reaper measures from
    /// "conversation went idle" not "row inserted."
    pub fn touch_active(&self, group_id: &str, now: i64) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_principal_groups SET last_active_at = ?1 \
                 WHERE group_id = ?2",
                params![now, group_id],
            )?;
            Ok(())
        })
    }

    /// Bind a `state_conversations.conversation_id` to a `group_id`.
    /// Idempotent — no-op when the column already matches.
    pub fn bind_conversation(&self, conversation_id: &str, group_id: &str) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_conversations \
                 SET principal_group_id = ?1 \
                 WHERE conversation_id = ?2",
                params![group_id, conversation_id],
            )?;
            Ok(())
        })
    }

    /// Reverse of [`bind_conversation`]: read the
    /// `state_conversations.principal_group_id` column for a given
    /// conversation. Returns `Ok(None)` when the row exists but the
    /// column is NULL (a controller-initiated conversation that has
    /// no transport binding yet) AND when no row exists at all —
    /// callers don't need to distinguish, and conflating spares one
    /// query path.
    ///
    /// Phase 4 hot path: `tool_dispatch::build_ctx_for` calls this
    /// every time the dispatcher hands a `Capability::Transport`
    /// tool a `ToolCtx`, so it composes the per-turn lookup that
    /// resolves `signal.reply`'s recipient. The query hits the
    /// conversation-id PK so the cost is one B-tree seek.
    pub fn principal_group_id_for(&self, conversation_id: &str) -> Result<Option<String>, DbError> {
        self.db.with_conn(|c| {
            let result: Result<Option<String>, rusqlite::Error> = c.query_row(
                "SELECT principal_group_id FROM state_conversations \
                 WHERE conversation_id = ?1",
                params![conversation_id],
                |r| r.get::<_, Option<String>>(0),
            );
            match result {
                Ok(opt) => Ok(opt),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(DbError::from(e)),
            }
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

    fn pids(s: &[&str]) -> Vec<PrincipalId> {
        s.iter().map(|x| PrincipalId::from(*x)).collect()
    }

    #[test]
    fn principal_set_hash_is_order_independent() {
        let a = principal_set_hash(&pids(&["alice", "bob", "carol"]));
        let b = principal_set_hash(&pids(&["carol", "alice", "bob"]));
        let c = principal_set_hash(&pids(&["bob", "alice", "carol"]));
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn principal_set_hash_dedupes_duplicates() {
        let with_dup = principal_set_hash(&pids(&["alice", "alice", "bob"]));
        let without = principal_set_hash(&pids(&["alice", "bob"]));
        assert_eq!(with_dup, without);
    }

    #[test]
    fn principal_set_hash_distinguishes_different_sets() {
        let a = principal_set_hash(&pids(&["alice", "bob"]));
        let b = principal_set_hash(&pids(&["alice", "carol"]));
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_creates_new_row_on_first_sighting() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let principals = pids(&["controller"]);
        let key = GroupKey {
            channel: "web",
            native_group_id: None,
            principals: &principals,
            includes_controller: true,
        };
        let g = store.resolve(&key, 1000).unwrap();
        assert_eq!(g.channel, "web");
        assert!(g.native_group_id.is_none());
        assert!(g.includes_controller);
        assert_eq!(g.created_at, 1000);
    }

    #[test]
    fn resolve_returns_same_row_for_same_key() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let principals = pids(&["controller"]);
        let key = GroupKey {
            channel: "web",
            native_group_id: None,
            principals: &principals,
            includes_controller: true,
        };
        let g1 = store.resolve(&key, 1000).unwrap();
        let g2 = store.resolve(&key, 2000).unwrap();
        assert_eq!(g1.group_id, g2.group_id);
        // last_active_at advanced.
        assert_eq!(g2.last_active_at, 2000);
    }

    #[test]
    fn resolve_returns_same_row_regardless_of_principal_order() {
        // Reaffirm the order-independence at the resolve layer
        // (the hash already does it but the SQL lookup is the
        // load-bearing seam).
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let p_abc = pids(&["alice", "bob", "carol"]);
        let p_cba = pids(&["carol", "bob", "alice"]);
        let g1 = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &p_abc,
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        let g2 = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &p_cba,
                    includes_controller: false,
                },
                2000,
            )
            .unwrap();
        assert_eq!(g1.group_id, g2.group_id);
    }

    #[test]
    fn resolve_distinguishes_different_principal_sets_on_same_channel() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let bob = pids(&["bob"]);
        let bob_and_carol = pids(&["bob", "carol"]);
        let g1 = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &bob,
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        let g2 = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &bob_and_carol,
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        assert_ne!(g1.group_id, g2.group_id);
    }

    #[test]
    fn resolve_distinguishes_same_principals_across_channels() {
        // Bob on Signal vs Bob on WhatsApp = two runners.
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let bob = pids(&["bob"]);
        let g1 = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &bob,
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        let g2 = store
            .resolve(
                &GroupKey {
                    channel: "whatsapp",
                    native_group_id: None,
                    principals: &bob,
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        assert_ne!(g1.group_id, g2.group_id);
    }

    #[test]
    fn native_group_id_wins_over_principal_set() {
        // The controller-only WhatsApp group starts as `(whatsapp,
        // jid=foo, {controller})`. Then Alice joins. The native
        // jid stays the same → same group_id, but membership
        // shifts. Verifies "channel-native id is authoritative."
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let just_controller = pids(&["controller"]);
        let with_alice = pids(&["controller", "alice"]);
        let g1 = store
            .resolve(
                &GroupKey {
                    channel: "whatsapp",
                    native_group_id: Some("jid-foo"),
                    principals: &just_controller,
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        let g2 = store
            .resolve(
                &GroupKey {
                    channel: "whatsapp",
                    native_group_id: Some("jid-foo"),
                    principals: &with_alice,
                    includes_controller: true,
                },
                2000,
            )
            .unwrap();
        assert_eq!(g1.group_id, g2.group_id);

        // Membership reconciled.
        let mems = store.members(&g1.group_id).unwrap();
        let names: Vec<&str> = mems.iter().map(PrincipalId::as_str).collect();
        assert_eq!(names, vec!["alice", "controller"]);
    }

    #[test]
    fn includes_controller_is_sticky_true() {
        // Once a group has had the controller in it, removing them
        // shouldn't flip the flag. (Belt-and-suspenders against a
        // membership-glitch path stripping the controller pin.)
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let with = pids(&["controller", "alice"]);
        let without = pids(&["alice"]);
        let g1 = store
            .resolve(
                &GroupKey {
                    channel: "whatsapp",
                    native_group_id: Some("jid-bar"),
                    principals: &with,
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        assert!(g1.includes_controller);

        let g2 = store
            .resolve(
                &GroupKey {
                    channel: "whatsapp",
                    native_group_id: Some("jid-bar"),
                    principals: &without,
                    includes_controller: false,
                },
                2000,
            )
            .unwrap();
        assert!(g2.includes_controller, "flag must remain true once set");
    }

    #[test]
    fn delete_removes_row_and_members() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let g = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: Some("g123"),
                    principals: &pids(&["alice", "bob"]),
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        assert_eq!(store.members(&g.group_id).unwrap().len(), 2);
        assert!(store.delete(&g.group_id).unwrap());
        assert!(store.get(&g.group_id).unwrap().is_none());
        assert_eq!(store.members(&g.group_id).unwrap().len(), 0);
        // Idempotent on second call.
        assert!(!store.delete(&g.group_id).unwrap());
    }

    #[test]
    fn add_member_grows_membership_idempotently() {
        // The per-message routing path mints a group with empty
        // members and then accumulates senders one inbound at a
        // time via `add_member`. The contract here:
        //
        //   * first call inserts the row + bumps last_active_at
        //   * second call with the same id is a clean no-op (no
        //     duplicate row, no error)
        //   * inserting a different principal grows the count
        //
        // Pinning this is the regression guard for "the agent keeps
        // barging into group conversations" — a fix that's silently
        // a no-op (e.g., a future refactor that drops the OR IGNORE
        // clause) would re-introduce the bug.
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let g = store
            .resolve(
                &GroupKey {
                    channel: "whatsapp",
                    native_group_id: Some("group-jid-x"),
                    principals: &[],
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        assert_eq!(
            store.members(&g.group_id).unwrap().len(),
            0,
            "fresh group with empty principals must start with 0 members"
        );

        let alice = PrincipalId::from("alice".to_owned());
        let bob = PrincipalId::from("bob".to_owned());

        store.add_member(&g.group_id, &alice, 1100).unwrap();
        let m1 = store.members(&g.group_id).unwrap();
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].as_str(), "alice");
        let after1 = store.get(&g.group_id).unwrap().unwrap();
        assert_eq!(
            after1.last_active_at, 1100,
            "add_member must bump last_active_at"
        );

        // Second call with the same id is a no-op — no duplicate.
        store.add_member(&g.group_id, &alice, 1200).unwrap();
        let m2 = store.members(&g.group_id).unwrap();
        assert_eq!(m2.len(), 1, "duplicate add must be a no-op");

        // Different principal grows the count.
        store.add_member(&g.group_id, &bob, 1300).unwrap();
        let m3 = store.members(&g.group_id).unwrap();
        assert_eq!(m3.len(), 2);
    }

    #[test]
    fn touch_active_advances_timestamp_only() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let g = store
            .resolve(
                &GroupKey {
                    channel: "web",
                    native_group_id: None,
                    principals: &pids(&["controller"]),
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        store.touch_active(&g.group_id, 9999).unwrap();
        let after = store.get(&g.group_id).unwrap().unwrap();
        assert_eq!(after.last_active_at, 9999);
        assert_eq!(after.created_at, 1000);
        assert!(after.includes_controller);
    }

    #[test]
    fn list_all_orders_by_recent_activity() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let _g_old = store
            .resolve(
                &GroupKey {
                    channel: "web",
                    native_group_id: None,
                    principals: &pids(&["controller"]),
                    includes_controller: true,
                },
                100,
            )
            .unwrap();
        let _g_recent = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &pids(&["bob"]),
                    includes_controller: false,
                },
                500,
            )
            .unwrap();
        let listed = store.list_all().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].last_active_at >= listed[1].last_active_at);
    }

    #[test]
    fn bind_conversation_sets_foreign_key() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        // Need a state_conversations row to bind to.
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_conversations \
                 (conversation_id, kind, last_seq, phase, trust_class) \
                 VALUES ('conv-1', 'ControllerDM', 0, 'idle', 'Controller')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let g = store
            .resolve(
                &GroupKey {
                    channel: "web",
                    native_group_id: None,
                    principals: &pids(&["controller"]),
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        store.bind_conversation("conv-1", &g.group_id).unwrap();
        // Read back.
        let bound: String = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT principal_group_id FROM state_conversations \
                     WHERE conversation_id = 'conv-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap())
            })
            .unwrap();
        assert_eq!(bound, g.group_id);
    }

    #[test]
    fn is_controller_solo_true_for_one_member_controller_dm() {
        // The web/SPA controller runner — single-member, includes
        // controller, must stay un-reapable.
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let g = store
            .resolve(
                &GroupKey {
                    channel: "web",
                    native_group_id: None,
                    principals: &pids(&["controller"]),
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        assert!(store.is_controller_solo(&g.group_id).unwrap());
    }

    #[test]
    fn is_controller_solo_false_for_multi_member_group_with_controller() {
        // The repro: a Signal group that the controller is a passive
        // member of. Pre-fix the supervisor flagged this as
        // `controller_runner=true` and the reaper skipped it forever.
        // These runners should follow the regular ephemeral
        // group-container lifecycle.
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let g = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: Some("signal:group:abc"),
                    principals: &pids(&["controller", "alice", "bob"]),
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();
        assert!(g.includes_controller);
        assert!(
            !store.is_controller_solo(&g.group_id).unwrap(),
            "multi-member groups must not be treated as controller-solo \
             even when the controller is a member"
        );
    }

    #[test]
    fn is_controller_solo_false_for_one_member_non_controller_dm() {
        // A DM with a non-controller principal: single member but
        // includes_controller=false, so not the controller solo runner.
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        let g = store
            .resolve(
                &GroupKey {
                    channel: "signal",
                    native_group_id: None,
                    principals: &pids(&["alice"]),
                    includes_controller: false,
                },
                1000,
            )
            .unwrap();
        assert!(!store.is_controller_solo(&g.group_id).unwrap());
    }

    #[test]
    fn is_controller_solo_false_for_unknown_group_id() {
        let db = fresh_db();
        let store = PrincipalGroupStore::new(&db);
        assert!(!store.is_controller_solo("does-not-exist").unwrap());
    }
}
