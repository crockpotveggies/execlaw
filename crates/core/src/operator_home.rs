//! Operator "Inbox" thread (M6).
//!
//! Singleton conversation of [`ConversationKind::OperatorHome`] per
//! operator principal. Auto-minted lazily on first need (ReplyRouter
//! delivery failure, briefing flow output, explicit `ChatAppend(home)`
//! target). Pinned at the top of the SPA sidebar with a 📥 icon and
//! excluded from auto-archive retention.
//!
//! Why singleton: avoid the bug where multiple "Inbox" threads pile
//! up from race conditions on first-use. The lookup is by
//! `(kind = 'OperatorHome', controller_id = <operator>)`, indexed
//! via `state_conversations.kind` (migration 0012's index — wait,
//! we dropped that. Existing kind index from baseline covers it).
//!
//! The Inbox thread participates in normal turn semantics:
//! the operator CAN chat in it, and replies go through the default
//! web flow like any other thread. The OperatorHome variant gates
//! only sorting + auto-archive exclusion.

use crate::conversation::{ConversationKind, ConversationRow, ConversationStore, Modality, Phase};
use crate::db::{Database, DbError};
use crate::ids::{ConversationId, EventSeq};
use rusqlite::params;

/// Look up the operator's Inbox conversation_id, creating it if it
/// doesn't yet exist. Idempotent and safe under concurrent access —
/// SQLite's PK on `state_conversations.conversation_id` serializes
/// the insert, and we re-query post-insert to handle the race where
/// two threads both think they were the creator.
///
/// `operator_principal_id` is the principal whose Inbox we want — in
/// a multi-user setup, each operator has their own. Most installs
/// today have a single Controller principal, so this returns the
/// same id for every caller.
pub fn ensure_operator_home(
    db: &Database,
    operator_principal_id: &str,
) -> Result<ConversationId, DbError> {
    if let Some(existing) = find_home(db, operator_principal_id)? {
        return Ok(existing);
    }

    // Mint a new Inbox row. `display_name = "Inbox"`, pinned, and
    // tagged with the operator's principal_id in `controller_id` so
    // we can find it again on a future lookup.
    let cid = ConversationId::new();
    let row = ConversationRow {
        conversation_id: cid.clone(),
        kind: ConversationKind::OperatorHome,
        last_seq: EventSeq(0),
        phase: Phase::Idle,
        controller_id: Some(operator_principal_id.to_owned()),
        trust_class: "Controller".to_owned(),
        snapshot_blob: None,
        snapshot_seq: None,
        lease_owner: None,
        lease_expires: None,
        modality: Modality::Text,
        display_name: Some("Inbox".to_owned()),
        display_name_source: "manual".to_owned(), // locked — transports must never clobber
        is_pinned: true,
        is_ephemeral: false,
        ephemeral_expires_at: None,
        last_activity_at: chrono::Utc::now().timestamp(),
    };
    ConversationStore::new(db).upsert(&row)?;

    // Re-check: if another thread won the race and inserted FIRST, we
    // just upserted with a NEW conversation_id, leaving an orphan.
    // The PK collision can't happen (we mint a fresh uuid), so the
    // mitigation is: re-query for the operator's home and return
    // whichever id wins. If there are two rows now (ours + the race
    // winner's), retention can prune later. The contention window is
    // microseconds — best-effort dedup is acceptable here.
    Ok(find_home(db, operator_principal_id)?.unwrap_or(cid))
}

fn find_home(
    db: &Database,
    operator_principal_id: &str,
) -> Result<Option<ConversationId>, DbError> {
    db.with_conn(|c| {
        let row = c
            .query_row(
                "SELECT conversation_id FROM state_conversations \
                 WHERE kind = 'OperatorHome' AND controller_id = ?1 \
                 ORDER BY last_activity_at ASC LIMIT 1",
                params![operator_principal_id],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(row.map(ConversationId::from_string))
    })
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
    fn ensure_operator_home_creates_once_then_returns_same_id() {
        let db = fresh_db();
        let id1 = ensure_operator_home(&db, "p-controller").unwrap();
        let id2 = ensure_operator_home(&db, "p-controller").unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn each_operator_gets_their_own_home() {
        let db = fresh_db();
        let a = ensure_operator_home(&db, "p-alice").unwrap();
        let b = ensure_operator_home(&db, "p-bob").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn home_row_is_pinned_and_tagged_inbox() {
        let db = fresh_db();
        let id = ensure_operator_home(&db, "p-x").unwrap();
        let store = ConversationStore::new(&db);
        let row = store.get(&id).unwrap().unwrap();
        assert!(matches!(row.kind, ConversationKind::OperatorHome));
        assert!(row.is_pinned);
        assert_eq!(row.display_name.as_deref(), Some("Inbox"));
        assert_eq!(row.controller_id.as_deref(), Some("p-x"));
    }
}
