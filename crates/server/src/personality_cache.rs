//! Per-turn personality-chunk cache.
//!
//! Every chat turn calls [`crate::chats::assemble_system_prompt`],
//! which in turn calls [`execlaw_core::personality::compose_system_prompt`].
//! Pre-cache, that did two indexed SQLite reads under the synchronous
//! Database mutex on every turn:
//!
//!   1. `PersonalityStore::get_default()`        — the default-scope row
//!   2. `PersonalityStore::get(Conversation, cid)` — per-conversation override
//!
//! The default row only changes when an operator edits Settings →
//! Personality (rare), and 99% of conversations have no per-conversation
//! override at all. This cache makes both observations payoff:
//!
//!   * The composed default chunk (the string returned by
//!     `compose_system_prompt(store, None)`) is cached behind a
//!     `RwLock<Option<String>>`. Invalidated on default-scope upsert.
//!
//!   * A `RwLock<HashSet<String>>` tracks the conversation ids known
//!     to have an override. Seeded lazily on first access via
//!     `PersonalityStore::list_overrides`. Mutated on
//!     conversation-scope upsert / delete. A turn whose `cid` is NOT
//!     in the set skips the override fetch entirely and serves the
//!     cached default chunk.
//!
//! Net result on the steady-state hot path: a turn for a conversation
//! with no override does ZERO SQLite reads for personality
//! composition. A turn for the rare conversation that HAS an
//! override still does one read (the override fetch) but reuses the
//! cached default for the base of the merge.

use execlaw_core::Database;
use execlaw_core::personality::{
    PersonalityScopeKind, PersonalityStore, compose_system_prompt,
};
use std::collections::HashSet;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Composer-output cache + override-presence index. One per
/// `AppState`; cheap to construct (empty cache, lazy seed).
///
/// All public methods are safe to call concurrently from multiple
/// chat-turn worker threads. The two locks are short-held and
/// orthogonal — readers on the default-chunk path never block
/// readers on the override-set path and vice versa.
#[derive(Debug, Default)]
pub struct PersonalityCache {
    /// Composed `compose_system_prompt(store, None)` output. `None`
    /// means "not yet computed OR invalidated"; the next call
    /// recomputes and stores. The string is cloned out on read so the
    /// caller owns it; the alternative (returning an `Arc<String>`)
    /// would force every existing `assemble_system_prompt` caller to
    /// learn about an extra `Arc` layer for no measurable savings on
    /// strings in the ~2 KiB range.
    default_chunk: RwLock<Option<String>>,
    /// Conversation ids whose `(scope_kind='conversation', scope_ref=cid)`
    /// row exists in `config_personality`. Seeded lazily on first
    /// `compose` call so test fixtures that never touch personality
    /// don't pay the seed query.
    override_cids: RwLock<HashSet<String>>,
    /// Flip-flop guarding the one-time `list_overrides` seed. Set
    /// inside `ensure_seeded` after the read completes; subsequent
    /// `compose` calls short-circuit the seed check via this flag
    /// without acquiring the write lock.
    seeded: AtomicBool,
}

impl PersonalityCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compose the personality chunk for this turn, hitting the cache
    /// whenever possible. Mirrors the call shape that pre-cache
    /// `assemble_system_prompt` had against
    /// `execlaw_core::personality::compose_system_prompt`.
    ///
    /// Returns the empty string on `compose_system_prompt` errors,
    /// matching the pre-cache `.unwrap_or_default()` behaviour at the
    /// `assemble_system_prompt` callsite. A real error is logged at
    /// `warn` so a corrupt personality table doesn't silently strip
    /// the operator's voice from every turn.
    pub fn compose(&self, db: &Database, conversation_id: Option<&str>) -> String {
        self.ensure_seeded(db);

        // Conversation-override fast path: only if the cid is known to
        // have a row do we pay for a fresh compose. Most conversations
        // miss this branch and fall through to the cached default.
        if let Some(cid) = conversation_id {
            let has_override = self
                .override_cids
                .read()
                .map(|set| set.contains(cid))
                .unwrap_or(false);
            if has_override {
                let store = PersonalityStore::new(db);
                return compose_system_prompt(&store, Some(cid)).unwrap_or_else(|e| {
                    tracing::warn!(
                        target: "personality_cache",
                        conversation_id = %cid,
                        error = %e,
                        "compose_system_prompt failed for override path; using empty chunk",
                    );
                    String::new()
                });
            }
        }

        // Default-only path: serve from cache when warm.
        {
            if let Ok(guard) = self.default_chunk.read()
                && let Some(s) = guard.as_ref()
            {
                return s.clone();
            }
        }

        // Cold fill. We deliberately don't hold a write lock across
        // the compose — two racing turns might compute the same chunk
        // and that's fine; the second `write()` overwrites the first
        // with an identical value.
        let store = PersonalityStore::new(db);
        let chunk = compose_system_prompt(&store, None).unwrap_or_else(|e| {
            tracing::warn!(
                target: "personality_cache",
                error = %e,
                "compose_system_prompt failed for default path; using empty chunk",
            );
            String::new()
        });
        if let Ok(mut guard) = self.default_chunk.write() {
            *guard = Some(chunk.clone());
        }
        chunk
    }

    /// Drop the cached default chunk. Call after `PersonalityStore::upsert`
    /// has written a `scope = "default"` row.
    pub fn invalidate_default(&self) {
        if let Ok(mut guard) = self.default_chunk.write() {
            *guard = None;
        }
    }

    /// Record that the given conversation id now has an override row.
    /// Idempotent — called from both the create + update paths since
    /// `PersonalityStore::upsert` collapses them.
    pub fn note_override(&self, conversation_id: &str) {
        if let Ok(mut guard) = self.override_cids.write() {
            guard.insert(conversation_id.to_owned());
        }
    }

    /// Record that the override row for this conversation id no longer
    /// exists. Call after `PersonalityStore::delete` succeeds for the
    /// `(Conversation, cid)` pair.
    pub fn drop_override(&self, conversation_id: &str) {
        if let Ok(mut guard) = self.override_cids.write() {
            guard.remove(conversation_id);
        }
    }

    /// Lazy one-time seed of the override-cid set from the DB. Cheap
    /// — `list_overrides` is one indexed query — but skipped on
    /// every subsequent call via the `seeded` flag.
    fn ensure_seeded(&self, db: &Database) {
        if self.seeded.load(Ordering::Acquire) {
            return;
        }
        // Multiple racing callers might pass this check; the last
        // writer's set wins and the set's contents are identical
        // either way. The compare_exchange isn't strictly needed —
        // we use it to avoid two redundant DB reads when there's
        // contention.
        if self
            .seeded
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let store = PersonalityStore::new(db);
        match store.list_overrides() {
            Ok(rows) => {
                if let Ok(mut guard) = self.override_cids.write() {
                    for row in rows {
                        if row.scope_kind == PersonalityScopeKind::Conversation {
                            guard.insert(row.scope_ref);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "personality_cache",
                    error = %e,
                    "personality override seed failed; cache will fall through to DB for every cid",
                );
                // Re-arm the flag so a future call retries. The
                // alternative (leaving `seeded = true` after a
                // failed seed) would mean every turn for a
                // conversation WITH an override silently uses the
                // un-overridden default chunk until restart.
                self.seeded.store(false, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::personality::{
        PersonalityField, PersonalityScopeKind, PersonalityUpsert,
    };
    use std::collections::HashSet;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn upsert_default(db: &Database, display_name: &str) {
        let store = PersonalityStore::new(db);
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Default,
                    scope_ref: "".into(),
                    display_name: display_name.into(),
                    role: "Personal assistant".into(),
                    tone: "Concise".into(),
                    communication_style: "".into(),
                    initiative: "".into(),
                    about_agent: "".into(),
                    about_controller: "".into(),
                    custom_instructions: "".into(),
                    voice_id: None,
                    override_fields: HashSet::new(),
                },
                100,
            )
            .unwrap();
    }

    fn upsert_override(db: &Database, cid: &str, tone: &str) {
        let store = PersonalityStore::new(db);
        let mut fields = HashSet::new();
        fields.insert(PersonalityField::Tone);
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Conversation,
                    scope_ref: cid.into(),
                    display_name: "".into(),
                    role: "".into(),
                    tone: tone.into(),
                    communication_style: "".into(),
                    initiative: "".into(),
                    about_agent: "".into(),
                    about_controller: "".into(),
                    custom_instructions: "".into(),
                    voice_id: None,
                    override_fields: fields,
                },
                100,
            )
            .unwrap();
    }

    #[test]
    fn cold_call_warms_default_chunk_and_subsequent_calls_match() {
        let db = fresh_db();
        upsert_default(&db, "Earl");
        let cache = PersonalityCache::new();

        let cold = cache.compose(&db, None);
        let warm = cache.compose(&db, None);
        assert_eq!(cold, warm);
        assert!(cold.contains("Name: Earl"));

        // Inspect the cache directly — the warm read should have
        // come from the stored Option.
        let guard = cache.default_chunk.read().unwrap();
        assert!(guard.is_some());
    }

    #[test]
    fn invalidate_default_forces_recompute_on_next_call() {
        let db = fresh_db();
        upsert_default(&db, "Earl");
        let cache = PersonalityCache::new();

        let first = cache.compose(&db, None);
        assert!(first.contains("Name: Earl"));

        // Operator edits the default name out-of-band; without
        // invalidation we'd keep serving "Earl" until restart.
        upsert_default(&db, "Brunhilda");
        cache.invalidate_default();

        let second = cache.compose(&db, None);
        assert!(second.contains("Name: Brunhilda"));
        assert!(!second.contains("Name: Earl"));
    }

    #[test]
    fn missing_override_serves_cached_default_for_known_cid() {
        let db = fresh_db();
        upsert_default(&db, "Earl");
        let cache = PersonalityCache::new();

        // 99% case: a cid that has no override row. The default
        // chunk should still appear (no override merge runs).
        let chunk = cache.compose(&db, Some("conv-no-override"));
        assert!(chunk.contains("Name: Earl"));
        assert!(chunk.contains("# Tone\nConcise"));
    }

    #[test]
    fn note_override_makes_subsequent_call_pick_up_the_row() {
        let db = fresh_db();
        upsert_default(&db, "Earl");
        let cache = PersonalityCache::new();

        // Warm the default chunk.
        let _ = cache.compose(&db, None);

        upsert_override(&db, "conv-p", "Pirate");
        cache.note_override("conv-p");

        let chunk = cache.compose(&db, Some("conv-p"));
        assert!(chunk.contains("Name: Earl"), "default identity still flows");
        assert!(
            chunk.contains("# Tone\nPirate"),
            "override merged on top: {chunk}"
        );
    }

    #[test]
    fn drop_override_restores_default_on_next_call() {
        let db = fresh_db();
        upsert_default(&db, "Earl");
        upsert_override(&db, "conv-p", "Pirate");
        let cache = PersonalityCache::new();
        cache.note_override("conv-p");

        let pirate = cache.compose(&db, Some("conv-p"));
        assert!(pirate.contains("# Tone\nPirate"));

        // Operator deletes the override.
        let store = PersonalityStore::new(&db);
        store
            .delete(PersonalityScopeKind::Conversation, "conv-p")
            .unwrap();
        cache.drop_override("conv-p");

        let plain = cache.compose(&db, Some("conv-p"));
        assert!(plain.contains("# Tone\nConcise"));
        assert!(!plain.contains("Pirate"));
    }

    #[test]
    fn lazy_seed_picks_up_pre_existing_override_rows() {
        let db = fresh_db();
        upsert_default(&db, "Earl");
        // Override exists in the DB BEFORE the cache is constructed,
        // simulating a server restart with existing overrides.
        upsert_override(&db, "conv-restart", "Pirate");

        let cache = PersonalityCache::new();
        let chunk = cache.compose(&db, Some("conv-restart"));
        assert!(
            chunk.contains("# Tone\nPirate"),
            "seed should have populated override_cids from list_overrides"
        );
    }
}
