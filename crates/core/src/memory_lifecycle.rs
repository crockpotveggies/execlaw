//! Memory tier promotion proposals + structured reflection log.
//!
//! These two stores back the "self-improving" half of the agent
//! model (see `docs/agent-model.md`):
//!
//!   * [`PromotionStore`] — proposed `memory_entries.tier` transitions
//!     awaiting a controller decision (or trust-policy auto-approve).
//!     The promotion sweeper inserts proposals; approval flips the
//!     target row's tier and stamps `decided_at`.
//!
//!   * [`ReflectionStore`] — append-only log of structured
//!     CONTEXT / REFLECTION / LESSON entries emitted by the post-turn
//!     reflection pass. Each row is anchored to the model_turn event
//!     it reflects on, so the audit trail (HMAC-chained `state_events`)
//!     stays the source of truth and reflections are derivative.
//!
//! Both stores enforce additive-only writes from the agent's
//! perspective. Decisions on proposals require a separate code path
//! that authenticates the actor (controller via SPA, or a trust-
//! policy rule) — the agent's tools never call `approve` / `reject`.

use crate::db::{Database, DbError};
use crate::memory::{MemoryStore, MemoryTier};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromotionReason {
    /// Sweeper found `hits >= threshold` within the look-back window.
    Frequency,
    /// Sweeper found `tier='hot'` row idle past the demotion deadline.
    Recency,
    /// Planner-role reflection pass proposed it from the lesson text.
    Reflection,
    /// Controller pinned/unpinned via the SPA's memory page.
    Manual,
}

impl PromotionReason {
    fn as_sql(self) -> &'static str {
        match self {
            PromotionReason::Frequency => "frequency",
            PromotionReason::Recency => "recency",
            PromotionReason::Reflection => "reflection",
            PromotionReason::Manual => "manual",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "frequency" => Some(Self::Frequency),
            "recency" => Some(Self::Recency),
            "reflection" => Some(Self::Reflection),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProposedBy {
    Sweeper,
    Planner,
    Controller,
}

impl ProposedBy {
    fn as_sql(self) -> &'static str {
        match self {
            ProposedBy::Sweeper => "sweeper",
            ProposedBy::Planner => "planner",
            ProposedBy::Controller => "controller",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromotionDecision {
    Approved,
    Rejected,
}

impl PromotionDecision {
    fn as_sql(self) -> &'static str {
        match self {
            PromotionDecision::Approved => "approved",
            PromotionDecision::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionProposal {
    pub id: i64,
    pub scope: String,
    pub trust_class: String,
    pub key: String,
    pub from_tier: MemoryTier,
    pub to_tier: MemoryTier,
    pub reason: PromotionReason,
    pub proposed_by: ProposedBy,
    pub proposed_at: i64,
    pub decided_at: Option<i64>,
    pub decision: Option<PromotionDecision>,
    pub decision_note: Option<String>,
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("proposal {0} not found")]
    NotFound(i64),
    #[error("proposal {0} already decided")]
    AlreadyDecided(i64),
    #[error("target row missing for proposal {0}")]
    TargetMissing(i64),
}

/// Insert / inspect / decide promotion proposals.
pub struct PromotionStore<'db> {
    db: &'db Database,
}

impl<'db> PromotionStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a fresh proposal. Idempotent against `(scope, trust, key, to_tier)`
    /// while a previous proposal for the same target tier is still
    /// pending — duplicate sweeper runs won't pile up dozens of
    /// identical pending rows. Returns the proposal id.
    pub fn propose(
        &self,
        scope: &str,
        trust_class: &str,
        key: &str,
        from_tier: MemoryTier,
        to_tier: MemoryTier,
        reason: PromotionReason,
        proposed_by: ProposedBy,
        now_unix: i64,
    ) -> Result<i64, LifecycleError> {
        // Fold duplicates: if an identical proposal is already pending,
        // return its id rather than creating a second.
        let existing: Option<i64> = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id FROM memory_promotions \
                 WHERE scope = ?1 AND trust_class = ?2 AND key = ?3 \
                   AND to_tier = ?4 AND decided_at IS NULL \
                 LIMIT 1",
                params![scope, trust_class, key, to_tier.as_sql()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
        })?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let id: i64 = self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO memory_promotions(\
                     scope, trust_class, key, from_tier, to_tier, \
                     reason, proposed_by, proposed_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    scope,
                    trust_class,
                    key,
                    from_tier.as_sql(),
                    to_tier.as_sql(),
                    reason.as_sql(),
                    proposed_by.as_sql(),
                    now_unix,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        Ok(id)
    }

    pub fn get(&self, id: i64) -> Result<Option<PromotionProposal>, LifecycleError> {
        let got = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id, scope, trust_class, key, from_tier, to_tier, \
                        reason, proposed_by, proposed_at, decided_at, decision, decision_note \
                 FROM memory_promotions WHERE id = ?1",
                params![id],
                row_to_proposal,
            )
            .optional()?)
        })?;
        Ok(got)
    }

    pub fn list_pending(&self, limit: u32) -> Result<Vec<PromotionProposal>, LifecycleError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, scope, trust_class, key, from_tier, to_tier, \
                        reason, proposed_by, proposed_at, decided_at, decision, decision_note \
                 FROM memory_promotions \
                 WHERE decided_at IS NULL \
                 ORDER BY proposed_at ASC \
                 LIMIT ?1",
            )?;
            let v = stmt
                .query_map(params![limit as i64], row_to_proposal)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })?;
        Ok(rows)
    }

    /// Approve a pending proposal: flip the target row's `tier` and
    /// stamp `decided_at`. Returns `LifecycleError::AlreadyDecided`
    /// if the proposal was already approved or rejected (idempotent
    /// from the caller's perspective — they should re-fetch state).
    pub fn approve(
        &self,
        id: i64,
        now_unix: i64,
        note: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let proposal = self.get(id)?.ok_or(LifecycleError::NotFound(id))?;
        if proposal.decided_at.is_some() {
            return Err(LifecycleError::AlreadyDecided(id));
        }
        // Apply the tier change first; the proposal flip records it.
        let store = MemoryStore::new(self.db);
        if store
            .get(&proposal.scope, &proposal.trust_class, &proposal.key)?
            .is_none()
        {
            return Err(LifecycleError::TargetMissing(id));
        }
        store.set_tier(
            &proposal.scope,
            &proposal.trust_class,
            &proposal.key,
            proposal.to_tier,
        )?;
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE memory_promotions \
                    SET decided_at = ?2, decision = ?3, decision_note = ?4 \
                  WHERE id = ?1 AND decided_at IS NULL",
                params![id, now_unix, PromotionDecision::Approved.as_sql(), note],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Reject a pending proposal without changing the target row.
    pub fn reject(&self, id: i64, now_unix: i64, note: Option<&str>) -> Result<(), LifecycleError> {
        let proposal = self.get(id)?.ok_or(LifecycleError::NotFound(id))?;
        if proposal.decided_at.is_some() {
            return Err(LifecycleError::AlreadyDecided(id));
        }
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE memory_promotions \
                    SET decided_at = ?2, decision = ?3, decision_note = ?4 \
                  WHERE id = ?1 AND decided_at IS NULL",
                params![id, now_unix, PromotionDecision::Rejected.as_sql(), note],
            )?;
            Ok(())
        })?;
        Ok(())
    }
}

fn row_to_proposal(r: &rusqlite::Row<'_>) -> rusqlite::Result<PromotionProposal> {
    use crate::memory::MemoryTier;
    let from_tier = MemoryTier::parse(&r.get::<_, String>(4)?).unwrap_or(MemoryTier::Warm);
    let to_tier = MemoryTier::parse(&r.get::<_, String>(5)?).unwrap_or(MemoryTier::Warm);
    let reason = PromotionReason::parse(&r.get::<_, String>(6)?).unwrap_or(PromotionReason::Manual);
    let proposed_by_str = r.get::<_, String>(7)?;
    let proposed_by = match proposed_by_str.as_str() {
        "sweeper" => ProposedBy::Sweeper,
        "planner" => ProposedBy::Planner,
        _ => ProposedBy::Controller,
    };
    let decision = r.get::<_, Option<String>>(10)?.as_deref().map(|s| match s {
        "approved" => PromotionDecision::Approved,
        _ => PromotionDecision::Rejected,
    });
    Ok(PromotionProposal {
        id: r.get(0)?,
        scope: r.get(1)?,
        trust_class: r.get(2)?,
        key: r.get(3)?,
        from_tier,
        to_tier,
        reason,
        proposed_by,
        proposed_at: r.get(8)?,
        decided_at: r.get(9)?,
        decision,
        decision_note: r.get(11)?,
    })
}

// =====================================================================
// ReflectionStore
// =====================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionEntry {
    pub id: i64,
    pub conversation_id: String,
    pub anchor_event_seq: i64,
    pub context_text: String,
    pub reflection_text: String,
    pub lesson_text: String,
    pub promotion_id: Option<i64>,
    pub created_at: i64,
}

pub struct ReflectionStore<'db> {
    db: &'db Database,
}

impl<'db> ReflectionStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Append a reflection. `promotion_id` may be `None` (the lesson
    /// was an observation that didn't propose a memory write).
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &self,
        conversation_id: &str,
        anchor_event_seq: i64,
        context_text: &str,
        reflection_text: &str,
        lesson_text: &str,
        promotion_id: Option<i64>,
        now_unix: i64,
    ) -> Result<i64, LifecycleError> {
        let id: i64 = self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO memory_reflections(\
                     conversation_id, anchor_event_seq, context_text, \
                     reflection_text, lesson_text, promotion_id, created_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    conversation_id,
                    anchor_event_seq,
                    context_text,
                    reflection_text,
                    lesson_text,
                    promotion_id,
                    now_unix,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        Ok(id)
    }

    pub fn list_for_conversation(
        &self,
        conversation_id: &str,
        limit: u32,
    ) -> Result<Vec<ReflectionEntry>, LifecycleError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, anchor_event_seq, context_text, \
                        reflection_text, lesson_text, promotion_id, created_at \
                 FROM memory_reflections \
                 WHERE conversation_id = ?1 \
                 ORDER BY created_at DESC \
                 LIMIT ?2",
            )?;
            let v = stmt
                .query_map(params![conversation_id, limit as i64], |r| {
                    Ok(ReflectionEntry {
                        id: r.get(0)?,
                        conversation_id: r.get(1)?,
                        anchor_event_seq: r.get(2)?,
                        context_text: r.get(3)?,
                        reflection_text: r.get(4)?,
                        lesson_text: r.get(5)?,
                        promotion_id: r.get(6)?,
                        created_at: r.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })?;
        Ok(rows)
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::memory::{MemoryEntry, MemoryStore};
    use crate::migrations::MigrationRunner;

    fn fresh() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn warm_row(db: &Database, scope: &str, trust: &str, key: &str) {
        MemoryStore::new(db)
            .upsert(&MemoryEntry {
                scope: scope.into(),
                trust_class: trust.into(),
                key: key.into(),
                value_blob: b"v".to_vec(),
                ttl_expires: None,
                updated_at: 1,
                tier: MemoryTier::Warm,
                hits: 0,
                last_used_at: None,
                created_at: 1,
            })
            .unwrap();
    }

    #[test]
    fn propose_returns_id_and_persists_row() {
        let db = fresh();
        warm_row(&db, "global", "Controller", "k");
        let store = PromotionStore::new(&db);
        let id = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        let got = store.get(id).unwrap().unwrap();
        assert_eq!(got.scope, "global");
        assert_eq!(got.from_tier, MemoryTier::Warm);
        assert_eq!(got.to_tier, MemoryTier::Hot);
        assert_eq!(got.reason, PromotionReason::Frequency);
        assert_eq!(got.proposed_by, ProposedBy::Sweeper);
        assert!(got.decided_at.is_none());
    }

    #[test]
    fn propose_is_idempotent_while_pending() {
        // Sweeper fires twice in the same window — second call must
        // return the existing pending proposal id, not insert a new
        // row, so the controller's approval queue doesn't fill with
        // duplicates.
        let db = fresh();
        warm_row(&db, "global", "Controller", "k");
        let store = PromotionStore::new(&db);
        let id1 = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        let id2 = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                200,
            )
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(store.list_pending(10).unwrap().len(), 1);
    }

    #[test]
    fn approve_flips_target_tier_and_stamps_decision() {
        let db = fresh();
        warm_row(&db, "global", "Controller", "k");
        let store = PromotionStore::new(&db);
        let id = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        store.approve(id, 200, Some("looks fine")).unwrap();
        let row = MemoryStore::new(&db)
            .get("global", "Controller", "k")
            .unwrap()
            .unwrap();
        assert_eq!(row.tier, MemoryTier::Hot);
        let p = store.get(id).unwrap().unwrap();
        assert_eq!(p.decided_at, Some(200));
        assert_eq!(p.decision, Some(PromotionDecision::Approved));
        assert_eq!(p.decision_note.as_deref(), Some("looks fine"));
    }

    #[test]
    fn reject_keeps_target_tier_and_stamps_decision() {
        let db = fresh();
        warm_row(&db, "global", "Controller", "k");
        let store = PromotionStore::new(&db);
        let id = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        store.reject(id, 200, Some("not sticky enough")).unwrap();
        let row = MemoryStore::new(&db)
            .get("global", "Controller", "k")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.tier,
            MemoryTier::Warm,
            "rejected proposal must NOT promote"
        );
        let p = store.get(id).unwrap().unwrap();
        assert_eq!(p.decision, Some(PromotionDecision::Rejected));
    }

    #[test]
    fn approve_twice_is_an_error() {
        // Idempotency is on the propose side, not the decide side.
        // Once a proposal is decided, re-deciding is rejected so a
        // bug in an approval handler can't silently re-flip the tier.
        let db = fresh();
        warm_row(&db, "global", "Controller", "k");
        let store = PromotionStore::new(&db);
        let id = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        store.approve(id, 200, None).unwrap();
        let err = store.approve(id, 300, None).unwrap_err();
        match err {
            LifecycleError::AlreadyDecided(n) => assert_eq!(n, id),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn approve_missing_target_row_errors() {
        // FK is ON DELETE CASCADE, so this only happens if the
        // target row was deleted between propose and approve. Rather
        // than silently no-op, fail loudly.
        let db = fresh();
        warm_row(&db, "global", "Controller", "k");
        let store = PromotionStore::new(&db);
        let id = store
            .propose(
                "global",
                "Controller",
                "k",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        // Delete the target row directly (cascade nukes the proposal too,
        // so we re-check that path separately). Here we suppress the
        // cascade by toggling FK enforcement off for the test:
        db.with_conn(|c| {
            c.execute("PRAGMA foreign_keys = OFF", []).unwrap();
            c.execute(
                "DELETE FROM memory_entries WHERE scope='global' AND trust_class='Controller' AND key='k'",
                [],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        let err = store.approve(id, 200, None).unwrap_err();
        match err {
            LifecycleError::TargetMissing(n) => assert_eq!(n, id),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn list_pending_orders_oldest_first_and_excludes_decided() {
        let db = fresh();
        warm_row(&db, "global", "Controller", "a");
        warm_row(&db, "global", "Controller", "b");
        warm_row(&db, "global", "Controller", "c");
        let store = PromotionStore::new(&db);
        let id_a = store
            .propose(
                "global",
                "Controller",
                "a",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                100,
            )
            .unwrap();
        let _id_b = store
            .propose(
                "global",
                "Controller",
                "b",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                200,
            )
            .unwrap();
        let id_c = store
            .propose(
                "global",
                "Controller",
                "c",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Frequency,
                ProposedBy::Sweeper,
                300,
            )
            .unwrap();
        store.approve(id_a, 400, None).unwrap();
        let pending = store.list_pending(10).unwrap();
        let keys: Vec<_> = pending.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["b", "c"]);
        assert!(
            pending
                .iter()
                .all(|p| p.id != id_a && p.id != id_c || p.id == id_c)
        );
    }

    // ---------------- ReflectionStore ----------------

    #[test]
    fn append_and_list_reflections_orders_newest_first() {
        let db = fresh();
        // Create a real proposal so the FK on promotion_id resolves.
        warm_row(&db, "global", "Controller", "voice");
        let prom_id = PromotionStore::new(&db)
            .propose(
                "global",
                "Controller",
                "voice",
                MemoryTier::Warm,
                MemoryTier::Hot,
                PromotionReason::Reflection,
                ProposedBy::Planner,
                150,
            )
            .unwrap();

        let r = ReflectionStore::new(&db);
        let id1 = r
            .append(
                "conv-1",
                10,
                "ctx-A",
                "saw the user correct the timezone twice",
                "always confirm timezone before scheduling",
                None,
                100,
            )
            .unwrap();
        let id2 = r
            .append(
                "conv-1",
                12,
                "ctx-B",
                "user pinned a preference",
                "remember 'bf_emma' as default voice",
                Some(prom_id),
                200,
            )
            .unwrap();
        let entries = r.list_for_conversation("conv-1", 10).unwrap();
        assert_eq!(entries.len(), 2);
        // Newest first.
        assert_eq!(entries[0].id, id2);
        assert_eq!(entries[1].id, id1);
        assert_eq!(entries[0].promotion_id, Some(prom_id));
        assert!(entries[1].promotion_id.is_none());
    }

    #[test]
    fn reflections_are_scoped_to_their_conversation() {
        let db = fresh();
        let r = ReflectionStore::new(&db);
        r.append("conv-A", 1, "x", "y", "z", None, 100).unwrap();
        r.append("conv-B", 1, "x", "y", "z", None, 100).unwrap();
        let a = r.list_for_conversation("conv-A", 10).unwrap();
        let b = r.list_for_conversation("conv-B", 10).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].conversation_id, "conv-A");
        assert_eq!(b[0].conversation_id, "conv-B");
    }
}
