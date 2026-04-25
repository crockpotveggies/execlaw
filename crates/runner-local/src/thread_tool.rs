//! `set_thread_name` agent tool.
//!
//! The SPA shows a 3-word title for every thread (Phase 6 sidebar).
//! For internal/new threads, the LLM writes that title via this tool
//! once it has enough context. For external groups the transport
//! plugin sets the title from the group name; for the controller's
//! pinned thread the title is hard-coded "Control thread".
//!
//! The tool is plain OpenAI function-calling — same shape as the
//! memory tools — so any backend that speaks the OpenAI chat-
//! completions schema can use it. The handler writes
//! `state_conversations.display_name` via `ConversationStore`; the
//! WS event bus picks up the change on the next read and the SPA
//! re-renders the sidebar.

use crate::memory_tool::{FunctionDef, ToolDefinition};
use execlaw_core::conversation::ConversationStore;
use execlaw_core::ids::ConversationId;
use serde::Deserialize;
use serde_json::json;

/// Tightest reasonable cap on the title length. Three short-to-medium
/// English words rarely exceed 30 chars; we allow up to 64 to give the
/// model room for proper nouns and multi-word names.
pub const MAX_THREAD_NAME_LEN: usize = 64;

/// The single `set_thread_name` tool definition.
pub fn thread_tool_definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        kind: "function".into(),
        function: FunctionDef {
            name: "set_thread_name".into(),
            description: "Set the human-readable title for the CURRENT thread. \
                          Use a concise 3-word summary that reflects the topic. \
                          Call this once enough context has accumulated; you can \
                          call it again later to refine."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The new thread title. Concise, ideally 3 words; max 64 chars."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
    }]
}

/// Errors the tool can produce. Bubbled back to the LLM as a failed
/// `tool_result` so it can self-correct.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ThreadToolError {
    #[error("set_thread_name: empty name (after trimming)")]
    Empty,
    #[error("set_thread_name: name too long ({0} chars; max 64)")]
    TooLong(usize),
    #[error("set_thread_name: bad args: {0}")]
    BadArgs(String),
    #[error("set_thread_name: db error: {0}")]
    Db(String),
}

#[derive(Debug, Deserialize)]
struct SetThreadNameArgs {
    name: String,
}

/// Dispatcher bound to one conversation. Construct per turn.
pub struct ThreadToolDispatcher<'db> {
    store: ConversationStore<'db>,
    conversation_id: ConversationId,
}

impl<'db> ThreadToolDispatcher<'db> {
    pub fn new(store: ConversationStore<'db>, conversation_id: ConversationId) -> Self {
        Self {
            store,
            conversation_id,
        }
    }

    /// Dispatch the tool call. Returns the tool_result JSON, or a
    /// typed error which the caller should encode into a failed
    /// `ToolResultPayload`.
    pub fn dispatch(
        &self,
        function_name: &str,
        args_json: &serde_json::Value,
    ) -> Result<serde_json::Value, ThreadToolError> {
        match function_name {
            "set_thread_name" => {
                let args: SetThreadNameArgs = serde_json::from_value(args_json.clone())
                    .map_err(|e| ThreadToolError::BadArgs(e.to_string()))?;
                self.set_name(&args.name)
            }
            other => Err(ThreadToolError::BadArgs(format!(
                "unknown thread tool: {other}"
            ))),
        }
    }

    fn set_name(&self, raw: &str) -> Result<serde_json::Value, ThreadToolError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ThreadToolError::Empty);
        }
        // char count, not byte count — the cap is about how it renders,
        // not how it's stored.
        let chars = trimmed.chars().count();
        if chars > MAX_THREAD_NAME_LEN {
            return Err(ThreadToolError::TooLong(chars));
        }
        self.store
            .set_display_name(&self.conversation_id, Some(trimmed))
            .map_err(|e| ThreadToolError::Db(e.to_string()))?;
        Ok(json!({"ok": true, "name": trimmed}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, Modality, Phase,
    };
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::ids::EventSeq;
    use execlaw_core::migrations::MigrationRunner;

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
            })
            .unwrap();
        cid
    }

    #[test]
    fn definitions_expose_set_thread_name() {
        let defs = thread_tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "set_thread_name");
        // JSON Schema requires `name`.
        assert_eq!(
            defs[0].function.parameters["required"][0],
            serde_json::json!("name")
        );
    }

    #[test]
    fn set_name_writes_display_name_to_store() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let store = ConversationStore::new(&db);
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());
        let out = dispatcher
            .dispatch("set_thread_name", &json!({"name": "Q4 budget review"}))
            .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["name"], "Q4 budget review");
        let row = store.get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Q4 budget review"));
    }

    /// Whitespace is trimmed before write. Pure-whitespace input is rejected.
    #[test]
    fn set_name_trims_whitespace_and_rejects_empty() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c2");
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());

        // Trims leading/trailing whitespace.
        dispatcher
            .dispatch("set_thread_name", &json!({"name": "  Trimmed  "}))
            .unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Trimmed"));

        // Empty / whitespace-only is rejected.
        let err = dispatcher
            .dispatch("set_thread_name", &json!({"name": "   "}))
            .unwrap_err();
        assert_eq!(err, ThreadToolError::Empty);
    }

    /// 65-char names are rejected; 64-char names are accepted (boundary).
    #[test]
    fn set_name_enforces_length_cap() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c3");
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());

        let ok_64 = "a".repeat(MAX_THREAD_NAME_LEN);
        dispatcher
            .dispatch("set_thread_name", &json!({"name": ok_64}))
            .unwrap();

        let too_long = "a".repeat(MAX_THREAD_NAME_LEN + 1);
        let err = dispatcher
            .dispatch("set_thread_name", &json!({"name": too_long}))
            .unwrap_err();
        assert_eq!(err, ThreadToolError::TooLong(MAX_THREAD_NAME_LEN + 1));
    }

    /// Unknown tool name routes to BadArgs — the runner's tool dispatch
    /// chain shouldn't get confused if the LLM hallucinates a sibling tool.
    #[test]
    fn unknown_tool_name_is_bad_args() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c4");
        let dispatcher = ThreadToolDispatcher::new(ConversationStore::new(&db), cid);
        let err = dispatcher
            .dispatch("delete_thread", &json!({"name": "x"}))
            .unwrap_err();
        match err {
            ThreadToolError::BadArgs(msg) => assert!(msg.contains("delete_thread")),
            other => panic!("expected BadArgs, got {other:?}"),
        }
    }

    /// Re-naming a thread overwrites the previous title.
    #[test]
    fn set_name_can_be_called_again() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c5");
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());
        dispatcher
            .dispatch("set_thread_name", &json!({"name": "First"}))
            .unwrap();
        dispatcher
            .dispatch("set_thread_name", &json!({"name": "Second"}))
            .unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Second"));
    }

    /// Multi-byte characters count as one each — a 3-emoji name fits
    /// well under the 64-char cap even though its byte length is large.
    #[test]
    fn set_name_counts_chars_not_bytes_for_cap() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c6");
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());
        let name = "📌📋💬"; // 3 chars, 12 bytes
        dispatcher
            .dispatch("set_thread_name", &json!({"name": name}))
            .unwrap();
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some(name));
    }

    /// Bad payload shape — missing `name` field — yields BadArgs not a panic.
    #[test]
    fn missing_name_field_is_bad_args() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c7");
        let dispatcher = ThreadToolDispatcher::new(ConversationStore::new(&db), cid);
        let err = dispatcher
            .dispatch("set_thread_name", &json!({"not_name": "x"}))
            .unwrap_err();
        assert!(matches!(err, ThreadToolError::BadArgs(_)));
    }
}
