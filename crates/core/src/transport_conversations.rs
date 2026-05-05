//! Transport→conversation routing (§2.6).
//!
//! Non-UI transports (Signal, email, voice, SMS) have no "new chat"
//! affordance — every inbound message just continues whatever thread
//! the principal already had. This module persists the mapping and
//! exposes [`ConversationResolver::resolve_or_mint`], which every
//! transport-tier plugin calls on every inbound message.
//!
//! Resolution order (matches MIGRATION_PLAN §2.6):
//!
//! 1. **Controller short-circuit.** If the resolved principal is the
//!    Controller, ALWAYS return the fixed [`controller_thread_id`].
//!    The controller has exactly one DM with the agent regardless of
//!    which channel the message arrived on; the SPA renders that one
//!    pinned "Control thread" by aggregating messages from every
//!    channel by their `channel_origin` payload field.
//! 2. **Idle-window lookup.** Otherwise, look up the `is_current = 1`
//!    row for the `(plugin_id, transport_handle, principal_id)` triple.
//!    If found and `now - last_message_at < idle_timeout_ms`, bump
//!    `last_message_at` to `now` and return the stored ConversationId.
//! 3. **Rotation.** Otherwise mark the old current row `is_current = 0`,
//!    mint a fresh ConversationId, INSERT a new row, and return it.
//!
//! Old (rotated) rows stay linked to their `principal_id` so the UI can
//! render "previous threads with X" without losing history.

use crate::db::{Database, DbError};
use crate::ids::ConversationId;
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// One row in `transport_conversations`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportConversationRow {
    pub plugin_id: String,
    pub transport_handle: String,
    pub principal_id: String,
    pub conversation_id: ConversationId,
    pub is_current: bool,
    pub last_message_at: i64,
}

/// Build the canonical Controller-thread ConversationId.
///
/// Stable per-controller; the SPA's "Control thread" sidebar entry
/// keys off this id. Hard-coded prefix so the id is recognizably a
/// controller thread even at a glance in logs.
pub fn controller_thread_id(controller_principal_id: &str) -> ConversationId {
    ConversationId::from(format!("controller-thread:{controller_principal_id}"))
}

/// CRUD facade for `transport_conversations`.
pub struct TransportConversationStore<'db> {
    db: &'db Database,
}

impl<'db> TransportConversationStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Look up the current row for the triple (`is_current = 1`).
    pub fn get_current(
        &self,
        plugin_id: &str,
        transport_handle: &str,
        principal_id: &str,
    ) -> Result<Option<TransportConversationRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT plugin_id, transport_handle, principal_id, conversation_id, \
                            is_current, last_message_at \
                     FROM transport_conversations \
                     WHERE plugin_id = ?1 AND transport_handle = ?2 AND principal_id = ?3 \
                       AND is_current = 1 \
                     LIMIT 1",
                    params![plugin_id, transport_handle, principal_id],
                    row_to_transport_conv,
                )
                .ok();
            Ok(got)
        })
    }

    /// Every row (current + rotated) for the principal across every
    /// transport. Used by the "previous threads with X" UI affordance.
    pub fn list_for_principal(
        &self,
        principal_id: &str,
    ) -> Result<Vec<TransportConversationRow>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT plugin_id, transport_handle, principal_id, conversation_id, \
                        is_current, last_message_at \
                 FROM transport_conversations \
                 WHERE principal_id = ?1 \
                 ORDER BY last_message_at DESC",
            )?;
            let rows = stmt
                .query_map(params![principal_id], row_to_transport_conv)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

fn row_to_transport_conv(r: &rusqlite::Row<'_>) -> rusqlite::Result<TransportConversationRow> {
    let is_current: i64 = r.get(4)?;
    Ok(TransportConversationRow {
        plugin_id: r.get(0)?,
        transport_handle: r.get(1)?,
        principal_id: r.get(2)?,
        conversation_id: ConversationId::from(r.get::<_, String>(3)?),
        is_current: is_current != 0,
        last_message_at: r.get(5)?,
    })
}

/// Inputs for `resolve_or_mint`.
#[derive(Debug, Clone)]
pub struct ResolveInput<'a> {
    pub plugin_id: &'a str,
    pub transport_handle: &'a str,
    pub principal_id: &'a str,
    /// True when `principal_id` is the operator-controller. Caller is
    /// the trust-resolution stage of the inbound pipeline; it already
    /// knows the trust class so it doesn't make sense to re-query here.
    pub is_controller: bool,
    /// Idle window after which a non-controller principal's next message
    /// rotates into a fresh thread. See MIGRATION_PLAN §2.6 for the
    /// recommended per-transport defaults.
    pub idle_timeout_ms: i64,
    /// `now` in unix seconds. Threaded explicitly so tests can pin time.
    pub now: i64,
}

/// Outcome of [`ConversationResolver::resolve_or_mint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// Controller short-circuit fired; returned id is the canonical
    /// Control-thread id.
    Controller(ConversationId),
    /// Existing within-idle-window row was reused.
    Continued(ConversationId),
    /// Old row aged out (or never existed); a new ConversationId was
    /// minted and inserted.
    Minted(ConversationId),
}

impl ResolveOutcome {
    pub fn conversation_id(&self) -> &ConversationId {
        match self {
            ResolveOutcome::Controller(id)
            | ResolveOutcome::Continued(id)
            | ResolveOutcome::Minted(id) => id,
        }
    }
}

/// The resolver itself. Holds nothing but a `&Database`; cheap to
/// construct per call.
pub struct ConversationResolver<'db> {
    db: &'db Database,
}

impl<'db> ConversationResolver<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Resolve an inbound message to a `ConversationId`, minting a
    /// fresh thread if the previous one aged out.
    ///
    /// All non-controller branches mutate the `transport_conversations`
    /// table; the entire decision lands in a single SQLite transaction
    /// so a crash mid-resolve can never leave the principal pointing at
    /// two `is_current = 1` rows.
    pub fn resolve_or_mint(&self, input: &ResolveInput<'_>) -> Result<ResolveOutcome, DbError> {
        // Controller short-circuit. No transaction needed — we don't
        // touch transport_conversations on the controller path.
        if input.is_controller {
            return Ok(ResolveOutcome::Controller(controller_thread_id(
                input.principal_id,
            )));
        }

        // The idle window is expressed in milliseconds in the public
        // API (matches every other transport-tier knob in execlaw)
        // even though `last_message_at` is unix seconds; scale here.
        let idle_secs = input.idle_timeout_ms / 1_000;

        self.db.transaction(|tx| {
            // 1. Look up the existing current row.
            let existing: Option<(String, i64)> = tx
                .query_row(
                    "SELECT conversation_id, last_message_at \
                     FROM transport_conversations \
                     WHERE plugin_id = ?1 AND transport_handle = ?2 \
                       AND principal_id = ?3 AND is_current = 1 \
                     LIMIT 1",
                    params![input.plugin_id, input.transport_handle, input.principal_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .ok();

            // 2. Idle-window check.
            if let Some((cid, last_at)) = existing.as_ref() {
                if input.now - last_at < idle_secs {
                    tx.execute(
                        "UPDATE transport_conversations \
                         SET last_message_at = ?1 \
                         WHERE plugin_id = ?2 AND transport_handle = ?3 \
                           AND principal_id = ?4 AND conversation_id = ?5",
                        params![
                            input.now,
                            input.plugin_id,
                            input.transport_handle,
                            input.principal_id,
                            cid,
                        ],
                    )?;
                    return Ok(ResolveOutcome::Continued(ConversationId::from(cid.clone())));
                }
            }

            // 3. Rotation: retire the old row (if any) and mint a fresh
            //    ConversationId.
            if existing.is_some() {
                tx.execute(
                    "UPDATE transport_conversations \
                     SET is_current = 0 \
                     WHERE plugin_id = ?1 AND transport_handle = ?2 \
                       AND principal_id = ?3 AND is_current = 1",
                    params![input.plugin_id, input.transport_handle, input.principal_id],
                )?;
            }

            let fresh = ConversationId::new();
            tx.execute(
                "INSERT INTO transport_conversations \
                 (plugin_id, transport_handle, principal_id, conversation_id, \
                  is_current, last_message_at) \
                 VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                params![
                    input.plugin_id,
                    input.transport_handle,
                    input.principal_id,
                    fresh.as_str(),
                    input.now,
                ],
            )?;
            Ok(ResolveOutcome::Minted(fresh))
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

    fn input<'a>(
        plugin: &'a str,
        handle: &'a str,
        principal: &'a str,
        is_controller: bool,
        now: i64,
    ) -> ResolveInput<'a> {
        ResolveInput {
            plugin_id: plugin,
            transport_handle: handle,
            principal_id: principal,
            is_controller,
            idle_timeout_ms: 60_000, // 60s, easy math
            now,
        }
    }

    #[test]
    fn controller_short_circuit_returns_canonical_id_without_db_writes() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        let outcome = resolver
            .resolve_or_mint(&input(
                "transport-signal",
                "signal:+15551234",
                "controller-1",
                true,
                100,
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ResolveOutcome::Controller(controller_thread_id("controller-1"))
        );

        // Same controller arriving on a different transport collapses
        // to the same id.
        let outcome2 = resolver
            .resolve_or_mint(&input(
                "transport-email",
                "email:c@example.com",
                "controller-1",
                true,
                200,
            ))
            .unwrap();
        assert_eq!(outcome2.conversation_id(), outcome.conversation_id());

        // Controller path must NOT touch transport_conversations.
        db.with_conn(|c| {
            let n: i64 = c
                .query_row("SELECT COUNT(*) FROM transport_conversations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(n, 0, "controller path should not write rows");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn first_inbound_from_outsider_mints_thread() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        let outcome = resolver
            .resolve_or_mint(&input(
                "transport-signal",
                "signal:+1555",
                "outsider-7",
                false,
                100,
            ))
            .unwrap();
        assert!(matches!(outcome, ResolveOutcome::Minted(_)));

        let store = TransportConversationStore::new(&db);
        let row = store
            .get_current("transport-signal", "signal:+1555", "outsider-7")
            .unwrap()
            .unwrap();
        assert!(row.is_current);
        assert_eq!(row.last_message_at, 100);
        assert_eq!(&row.conversation_id, outcome.conversation_id());
    }

    #[test]
    fn within_idle_window_continues_existing_thread() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);

        let first = resolver
            .resolve_or_mint(&input("p", "h", "outsider", false, 100))
            .unwrap();
        // 30s later — well within the 60s idle window.
        let second = resolver
            .resolve_or_mint(&input("p", "h", "outsider", false, 130))
            .unwrap();
        assert_eq!(first.conversation_id(), second.conversation_id());
        assert!(matches!(second, ResolveOutcome::Continued(_)));

        // last_message_at advanced.
        let store = TransportConversationStore::new(&db);
        let row = store.get_current("p", "h", "outsider").unwrap().unwrap();
        assert_eq!(row.last_message_at, 130);
    }

    #[test]
    fn past_idle_window_rotates_to_fresh_thread_and_retires_old_row() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);

        let first = resolver
            .resolve_or_mint(&input("p", "h", "outsider", false, 100))
            .unwrap();
        // 200s later — well past the 60s window.
        let second = resolver
            .resolve_or_mint(&input("p", "h", "outsider", false, 300))
            .unwrap();
        assert_ne!(first.conversation_id(), second.conversation_id());
        assert!(matches!(second, ResolveOutcome::Minted(_)));

        let store = TransportConversationStore::new(&db);
        // Two rows — the old one demoted to is_current = 0, the new one current.
        let all = store.list_for_principal("outsider").unwrap();
        assert_eq!(all.len(), 2);
        let current_count = all.iter().filter(|r| r.is_current).count();
        assert_eq!(current_count, 1, "exactly one current row per triple");

        let current = store.get_current("p", "h", "outsider").unwrap().unwrap();
        assert_eq!(&current.conversation_id, second.conversation_id());
    }

    /// Exact-boundary case: `now - last_message_at == idle_secs` is
    /// considered EXPIRED (`<` not `<=` in the rule). Documents the
    /// edge case so a future tweak doesn't silently change semantics.
    #[test]
    fn boundary_equal_idle_timeout_rotates() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        let _ = resolver
            .resolve_or_mint(&input("p", "h", "x", false, 1_000))
            .unwrap();
        // 1_000 + 60s exactly.
        let outcome = resolver
            .resolve_or_mint(&input("p", "h", "x", false, 1_060))
            .unwrap();
        assert!(matches!(outcome, ResolveOutcome::Minted(_)));
    }

    /// Different principals on the same transport handle are treated
    /// as distinct rows. (The triple `(plugin, handle, principal)` is
    /// the lookup key, not the pair.)
    #[test]
    fn different_principals_get_distinct_threads() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        let a = resolver
            .resolve_or_mint(&input("p", "shared-handle", "alice", false, 100))
            .unwrap();
        let b = resolver
            .resolve_or_mint(&input("p", "shared-handle", "bob", false, 100))
            .unwrap();
        assert_ne!(a.conversation_id(), b.conversation_id());
    }

    /// Resolver writes are atomic: a crash mid-rotation must never
    /// produce two `is_current = 1` rows. (Single-row check after
    /// many sequential rotations.)
    #[test]
    fn rotations_maintain_single_current_row_invariant() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        for tick in (100..=1_000).step_by(200) {
            resolver
                .resolve_or_mint(&input("p", "h", "x", false, tick))
                .unwrap();
        }
        let store = TransportConversationStore::new(&db);
        let all = store.list_for_principal("x").unwrap();
        let current = all.iter().filter(|r| r.is_current).count();
        assert_eq!(
            current, 1,
            "exactly one is_current row even after many rotations"
        );
    }

    #[test]
    fn list_for_principal_orders_by_recency_desc() {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        // First thread, then rotation past the window.
        resolver
            .resolve_or_mint(&input("p", "h", "x", false, 100))
            .unwrap();
        resolver
            .resolve_or_mint(&input("p", "h", "x", false, 1_000))
            .unwrap();

        let store = TransportConversationStore::new(&db);
        let all = store.list_for_principal("x").unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].last_message_at >= all[1].last_message_at);
    }

    #[test]
    fn controller_thread_id_is_stable_per_controller_id() {
        assert_eq!(
            controller_thread_id("ctrl-1"),
            controller_thread_id("ctrl-1")
        );
        assert_ne!(
            controller_thread_id("ctrl-1"),
            controller_thread_id("ctrl-2")
        );
        assert!(
            controller_thread_id("ctrl-1")
                .as_str()
                .starts_with("controller-thread:")
        );
    }
}
