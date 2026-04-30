//! DB-backed implementations of the `tool::*Api` capability traits.
//!
//! Each impl wraps the existing `Store` types and enforces caller-
//! trust + caller-conversation scoping internally. The tool authors
//! never see a `Database` handle — they hold an `Arc<dyn FooApi>`
//! and call its narrow methods.
//!
//! Production wiring: the dispatch layer constructs the impls per
//! call (cheap — they're thin wrappers over `Database` clones, which
//! are themselves `Arc`-based) and stuffs the right ones into the
//! `ToolCtx` based on what the tool's descriptor declared.
//!
//! 2026-04-29.

use crate::conversation::ConversationStore;
use crate::db::Database;
use crate::ids::ConversationId;
use crate::memory::{MemoryEntry, MemoryStore};
use crate::tool::{
    ApiError, ConversationApi, MemoryApi, MemoryListEntry, ThreadInfo,
};
use async_trait::async_trait;

// -----------------------------------------------------------------
// Trust ranking — local to this module so `core` stays free of
// `execlaw-policy`. The vocabulary intentionally matches `policy`'s
// `TrustLevel` so strings round-trip cleanly.
// -----------------------------------------------------------------

const TRUST_CLASSES_HIGH_TO_LOW: &[&str] = &[
    "Controller",
    "Delegated",
    "KnownTrusted",
    "KnownLimited",
    "UnknownPending",
    "Blocked",
];

fn trust_rank(class: &str) -> Option<u8> {
    TRUST_CLASSES_HIGH_TO_LOW
        .iter()
        .position(|&c| c == class)
        // Index 0 is the highest, but we want highest = highest rank.
        // Subtract from len-1 so Controller=5, Blocked=0.
        .map(|i| (TRUST_CLASSES_HIGH_TO_LOW.len() - 1 - i) as u8)
}

/// Whether a caller at `caller` is allowed to read memory tagged
/// `target`. Read-up is forbidden; read-at-or-below is allowed.
fn can_read(caller: &str, target: &str) -> bool {
    match (trust_rank(caller), trust_rank(target)) {
        (Some(a), Some(b)) => a >= b,
        // Unknown class strings are treated as the lowest possible
        // — never allowed to read anything labeled with a known
        // class. This is a conservative choice: a typo'd trust class
        // string at the dispatch layer fails closed.
        _ => false,
    }
}

/// Compute the chain of trust classes a caller can read, highest
/// first. Used by `MemoryApi::read` to cascade through the trust
/// classes the caller can see.
fn readable_classes(caller: &str) -> Vec<&'static str> {
    TRUST_CLASSES_HIGH_TO_LOW
        .iter()
        .copied()
        .filter(|c| can_read(caller, c))
        .collect()
}

// -----------------------------------------------------------------
// ConversationApi: DB-backed
// -----------------------------------------------------------------

/// Tightest reasonable cap on the thread display name — three short
/// English words rarely exceed 30 chars; we allow 64 for proper
/// nouns / multi-word names. Counted in chars (not bytes) so emoji
/// titles don't false-trip the cap.
pub const MAX_THREAD_DISPLAY_NAME_LEN: usize = 64;

/// DB-backed `ConversationApi`. Captures the caller's
/// `conversation_id` at construction so the trait methods can never
/// reach a different conversation.
pub struct DbConversationApi {
    db: Database,
    conversation_id: ConversationId,
}

impl DbConversationApi {
    pub fn new(db: Database, conversation_id: ConversationId) -> Self {
        Self {
            db,
            conversation_id,
        }
    }
}

#[async_trait]
impl ConversationApi for DbConversationApi {
    async fn get_thread(&self) -> Result<ThreadInfo, ApiError> {
        let db = self.db.clone();
        let cid = self.conversation_id.clone();
        let cid_for_err = cid.clone();
        let row = tokio::task::spawn_blocking(move || ConversationStore::new(&db).get(&cid))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(|e| ApiError::Storage(format!("conversation get: {e}")))?
            .ok_or_else(|| {
                ApiError::NotFound(format!("conversation {}", cid_for_err.as_str()))
            })?;
        Ok(ThreadInfo {
            conversation_id: row.conversation_id.as_str().to_owned(),
            display_name: row.display_name,
        })
    }

    async fn set_thread_name(&self, raw: &str) -> Result<(), ApiError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ApiError::Validation(
                "thread name is empty after trimming".into(),
            ));
        }
        let chars = trimmed.chars().count();
        if chars > MAX_THREAD_DISPLAY_NAME_LEN {
            return Err(ApiError::Validation(format!(
                "thread name too long ({chars} chars; max {MAX_THREAD_DISPLAY_NAME_LEN})"
            )));
        }
        let db = self.db.clone();
        let cid = self.conversation_id.clone();
        let name = trimmed.to_owned();
        tokio::task::spawn_blocking(move || {
            ConversationStore::new(&db).set_display_name(&cid, Some(&name))
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("set_display_name: {e}")))?;
        Ok(())
    }
}

// -----------------------------------------------------------------
// MemoryApi: DB-backed
// -----------------------------------------------------------------

/// DB-backed `MemoryApi`. Captures the caller's `caller_trust` at
/// construction so reads cascade through the right set of trust
/// classes and writes always land at the caller's level.
pub struct DbMemoryApi {
    db: Database,
    caller_trust: String,
    clock_now_unix: i64,
}

impl DbMemoryApi {
    pub fn new(db: Database, caller_trust: impl Into<String>, now_unix: i64) -> Self {
        Self {
            db,
            caller_trust: caller_trust.into(),
            clock_now_unix: now_unix,
        }
    }
}

#[async_trait]
impl MemoryApi for DbMemoryApi {
    async fn read(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<String>, ApiError> {
        let db = self.db.clone();
        let scope = scope.to_owned();
        let key = key.to_owned();
        let classes: Vec<&'static str> = readable_classes(&self.caller_trust);
        if classes.is_empty() {
            // Trust class string didn't parse to anything we know —
            // fail closed. Any unknown caller can't read anyone's memory.
            return Err(ApiError::NotAuthorized(format!(
                "trust class {:?} cannot read memory",
                self.caller_trust
            )));
        }
        let got = tokio::task::spawn_blocking(move || {
            let store = MemoryStore::new(&db);
            for class in classes {
                let entry = store.get(&scope, class, &key)?;
                if let Some(entry) = entry {
                    return Ok::<_, crate::DbError>(Some(entry));
                }
            }
            Ok(None)
        })
        .await
        .map_err(|e| ApiError::Storage(format!("join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("memory read: {e}")))?;

        match got {
            None => Ok(None),
            Some(entry) => {
                let s = String::from_utf8(entry.value_blob).map_err(|_| {
                    ApiError::Storage(
                        "stored memory value is not valid utf-8".into(),
                    )
                })?;
                Ok(Some(s))
            }
        }
    }

    async fn write(
        &self,
        scope: &str,
        key: &str,
        value: &str,
    ) -> Result<(), ApiError> {
        if trust_rank(&self.caller_trust).is_none() {
            return Err(ApiError::NotAuthorized(format!(
                "trust class {:?} cannot write memory",
                self.caller_trust
            )));
        }
        let entry = MemoryEntry {
            scope: scope.to_owned(),
            trust_class: self.caller_trust.clone(),
            key: key.to_owned(),
            value_blob: value.as_bytes().to_vec(),
            ttl_expires: None,
            updated_at: self.clock_now_unix,
        };
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || MemoryStore::new(&db).upsert(&entry))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(|e| ApiError::Storage(format!("memory write: {e}")))?;
        Ok(())
    }

    async fn list(
        &self,
        scope: &str,
        prefix: &str,
    ) -> Result<Vec<MemoryListEntry>, ApiError> {
        // The underlying `MemoryStore` doesn't have a scan method yet
        // (Phase 1 stub). Returning an empty list with the same shape
        // keeps the contract honest until that lands.
        let _ = (scope, prefix);
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{
        ConversationKind, ConversationRow, Modality, Phase,
    };
    use crate::db::DbConfig;
    use crate::ids::EventSeq;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_conversation(db: &Database, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(db)
            .upsert(&ConversationRow {
                conversation_id: cid.clone(),
                kind: ConversationKind::ControllerDM,
                last_seq: EventSeq(0),
                phase: Phase::Idle,
                controller_id: None,
                trust_class: "Controller".into(),
                snapshot_blob: None,
                snapshot_seq: None,
                lease_owner: None,
                lease_expires: None,
                modality: Modality::Text,
                display_name: None,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    // --- Trust ranking ------------------------------------------------

    #[test]
    fn trust_ranks_match_intended_order() {
        assert!(trust_rank("Controller").unwrap() > trust_rank("Delegated").unwrap());
        assert!(trust_rank("Delegated").unwrap() > trust_rank("KnownTrusted").unwrap());
        assert!(trust_rank("KnownTrusted").unwrap() > trust_rank("KnownLimited").unwrap());
        assert!(trust_rank("KnownLimited").unwrap() > trust_rank("UnknownPending").unwrap());
        assert!(trust_rank("UnknownPending").unwrap() > trust_rank("Blocked").unwrap());
    }

    #[test]
    fn unknown_trust_class_does_not_rank() {
        assert!(trust_rank("Goblin").is_none());
        assert!(trust_rank("").is_none());
    }

    #[test]
    fn can_read_enforces_no_read_up() {
        assert!(can_read("Controller", "Delegated"));
        assert!(can_read("Controller", "Controller"));
        assert!(!can_read("KnownTrusted", "Controller"));
        assert!(!can_read("Blocked", "Controller"));
        // Unknown caller never reads.
        assert!(!can_read("Goblin", "Controller"));
    }

    #[test]
    fn readable_classes_chain_is_caller_then_below() {
        let chain = readable_classes("KnownTrusted");
        assert_eq!(chain.first(), Some(&"KnownTrusted"));
        assert_eq!(chain.last(), Some(&"Blocked"));
        assert!(!chain.contains(&"Controller"));
        assert!(!chain.contains(&"Delegated"));
    }

    // --- ConversationApi ----------------------------------------------

    #[tokio::test]
    async fn conversation_api_get_returns_thread_info() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        ConversationStore::new(&db)
            .set_display_name(&cid, Some("First Topic"))
            .unwrap();
        let api = DbConversationApi::new(db, cid);
        let info = api.get_thread().await.unwrap();
        assert_eq!(info.conversation_id, "c1");
        assert_eq!(info.display_name.as_deref(), Some("First Topic"));
    }

    #[tokio::test]
    async fn conversation_api_set_name_writes_through() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c2");
        let api = DbConversationApi::new(db.clone(), cid.clone());
        api.set_thread_name("Q4 budget").await.unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Q4 budget"));
    }

    #[tokio::test]
    async fn conversation_api_set_name_trims_and_rejects_empty() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c3");
        let api = DbConversationApi::new(db.clone(), cid.clone());

        api.set_thread_name("  Trimmed  ").await.unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Trimmed"));

        match api.set_thread_name("   ").await.unwrap_err() {
            ApiError::Validation(_) => {}
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn conversation_api_set_name_enforces_64_char_cap() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c4");
        let api = DbConversationApi::new(db, cid);

        let ok_64 = "a".repeat(MAX_THREAD_DISPLAY_NAME_LEN);
        api.set_thread_name(&ok_64).await.unwrap();

        let too_long = "a".repeat(MAX_THREAD_DISPLAY_NAME_LEN + 1);
        match api.set_thread_name(&too_long).await.unwrap_err() {
            ApiError::Validation(msg) => assert!(msg.contains("too long")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Multi-byte chars count as 1 each — emoji titles stay legal even
    /// though their byte length is large.
    #[tokio::test]
    async fn conversation_api_counts_chars_not_bytes() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c5");
        let api = DbConversationApi::new(db.clone(), cid.clone());
        api.set_thread_name("📌📋💬").await.unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("📌📋💬"));
    }

    #[tokio::test]
    async fn conversation_api_get_missing_conversation_is_not_found() {
        let db = fresh_db();
        let api = DbConversationApi::new(db, ConversationId::from("nope"));
        match api.get_thread().await.unwrap_err() {
            ApiError::NotFound(s) => assert!(s.contains("nope")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // --- MemoryApi ----------------------------------------------------

    #[tokio::test]
    async fn memory_api_write_then_read_at_same_class() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db, "Controller", 0);
        api.write("global", "k", "hello").await.unwrap();
        let v = api.read("global", "k").await.unwrap();
        assert_eq!(v.as_deref(), Some("hello"));
    }

    /// Adversarial: low-trust caller cannot read controller memory.
    #[tokio::test]
    async fn memory_api_low_trust_cannot_read_controller() {
        let db = fresh_db();
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("global", "secret", "top-secret")
            .await
            .unwrap();
        let outsider = DbMemoryApi::new(db, "UnknownPending", 0);
        let v = outsider.read("global", "secret").await.unwrap();
        assert_eq!(v, None);
    }

    /// Writes always land at caller's class — model can't escalate by
    /// pretending. The capability layer doesn't even let the LLM
    /// supply a trust_class field; a faulty future caller that did
    /// would still be ignored because `caller_trust` is captured at
    /// construction.
    #[tokio::test]
    async fn memory_api_write_always_at_caller_class() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db.clone(), "KnownLimited", 0);
        api.write("s", "k", "v").await.unwrap();
        let store = MemoryStore::new(&db);
        assert!(store.get("s", "KnownLimited", "k").unwrap().is_some());
        assert!(store.get("s", "Controller", "k").unwrap().is_none());
    }

    /// Cascading reads: a Controller can read memories at every level
    /// down through Blocked. Higher-precedence (Controller) wins on
    /// conflicting keys.
    #[tokio::test]
    async fn memory_api_cascade_read_picks_highest_class_first() {
        let db = fresh_db();
        DbMemoryApi::new(db.clone(), "KnownTrusted", 0)
            .write("s", "k", "from-known-trusted")
            .await
            .unwrap();
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("s", "k", "from-controller")
            .await
            .unwrap();
        let v = DbMemoryApi::new(db, "Controller", 0)
            .read("s", "k")
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some("from-controller"));
    }

    #[tokio::test]
    async fn memory_api_unknown_trust_class_fails_closed_on_read() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db, "Goblin", 0);
        match api.read("s", "k").await.unwrap_err() {
            ApiError::NotAuthorized(_) => {}
            other => panic!("expected NotAuthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_api_unknown_trust_class_fails_closed_on_write() {
        let db = fresh_db();
        let api = DbMemoryApi::new(db, "Goblin", 0);
        match api.write("s", "k", "v").await.unwrap_err() {
            ApiError::NotAuthorized(_) => {}
            other => panic!("expected NotAuthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_api_list_is_currently_empty_stub() {
        let db = fresh_db();
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("s", "k1", "v")
            .await
            .unwrap();
        let v = DbMemoryApi::new(db, "Controller", 0)
            .list("s", "")
            .await
            .unwrap();
        assert!(v.is_empty());
    }
}
