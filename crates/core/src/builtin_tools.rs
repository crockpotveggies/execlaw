//! Core built-in tools, refactored onto the [`crate::tool::ToolImpl`]
//! trait.
//!
//! Each tool is a tiny, stateless struct: it owns its `ToolDescriptor`
//! and an `invoke` method that pulls the relevant capability API out
//! of the `ToolCtx` and calls a single method on it. The capability
//! impl is where the actual storage lookup, trust gating, and
//! validation live — see [`crate::tool_apis`].
//!
//! The shipped tools today:
//!
//! - `read_memory` — reads a key from the caller's trust scope, with
//!   read-down cascade.
//! - `write_memory` — writes a key at the caller's trust scope.
//! - `list_memory` — stub; returns an empty list (the underlying
//!   `MemoryStore` doesn't yet have a scan method).
//! - `set_thread_name` — writes `state_conversations.display_name` for
//!   the caller's conversation.
//! - `get_thread` — returns the caller's thread's metadata (display
//!   name, conversation id) so the agent can self-orient.
//!
//! Helper [`core_builtin_tools`] returns all of them as a single
//! `Vec<Arc<dyn ToolImpl>>` ready to register into the host's
//! `HookRegistry`. The same vec drives the boot-time
//! `config_tool_access` seeding via the descriptors'
//! `default_allowed_classes`.
//!
//! 2026-04-29.

use crate::tool::{
    Capability, NotifySeverity, ToolCtx, ToolDescriptor, ToolImpl, ToolLatency,
    ToolOutcome, ToolSource,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

// Default trust-class allowlists for the core built-ins. Memory tools
// are universally available because the read-down cascade and
// caller-scoped writes are themselves the security boundary; the
// access gate only needs to filter out `Blocked`. The conversation-
// metadata tools are the same — every active turn legitimately wants
// to know its own thread title.
fn default_allowed_for_memory() -> Vec<String> {
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
        "UnknownPending".into(),
    ]
}

fn default_allowed_for_conversation_read() -> Vec<String> {
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
        "UnknownPending".into(),
    ]
}

fn default_allowed_for_conversation_write() -> Vec<String> {
    // Renaming the thread is a write — keep it Controller + Delegated
    // by default. KnownTrusted contacts haven't proven authority over
    // labelling.
    vec!["Controller".into(), "Delegated".into()]
}

// ---------------------------------------------------------------
// read_memory
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReadMemoryArgs {
    scope: String,
    key: String,
}

pub struct ReadMemoryTool {
    descriptor: ToolDescriptor,
}

impl Default for ReadMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadMemoryTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "read_memory".into(),
                description:
                    "Read a memory value visible at the current conversation's trust scope. \
                     Returns the stored string, or null if nothing is stored under that key."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "description": "Memory scope, e.g. \"global\" or \"principal:<id>\"."
                        },
                        "key": {
                            "type": "string",
                            "description": "The memory key to look up."
                        }
                    },
                    "required": ["scope", "key"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::MemoryRead],
                default_allowed_classes: default_allowed_for_memory(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ReadMemoryTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ReadMemoryArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let memory = match ctx.memory.as_ref() {
            Some(m) => m,
            None => {
                return ToolOutcome::denied(
                    "memory capability not granted to this tool",
                );
            }
        };
        match memory.read(&args.scope, &args.key).await {
            Ok(Some(s)) => ToolOutcome::Ok(json!(s)),
            Ok(None) => ToolOutcome::Ok(Value::Null),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// write_memory
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WriteMemoryArgs {
    scope: String,
    key: String,
    value: String,
}

pub struct WriteMemoryTool {
    descriptor: ToolDescriptor,
}

impl Default for WriteMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteMemoryTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "write_memory".into(),
                description:
                    "Write a memory value at the current conversation's trust scope. \
                     Overwrites any previous value under the same scope + key."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "scope": {"type": "string"},
                        "key":   {"type": "string"},
                        "value": {"type": "string"}
                    },
                    "required": ["scope", "key", "value"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::MemoryWrite],
                default_allowed_classes: default_allowed_for_memory(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for WriteMemoryTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: WriteMemoryArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let memory = match ctx.memory.as_ref() {
            Some(m) => m,
            None => {
                return ToolOutcome::denied(
                    "memory capability not granted to this tool",
                );
            }
        };
        match memory.write(&args.scope, &args.key, &args.value).await {
            Ok(()) => ToolOutcome::Ok(json!({"ok": true})),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// list_memory (stub)
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListMemoryArgs {
    scope: String,
    #[serde(default)]
    prefix: String,
}

pub struct ListMemoryTool {
    descriptor: ToolDescriptor,
}

impl Default for ListMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListMemoryTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "list_memory".into(),
                description:
                    "List memory keys starting with `prefix` (or all keys if empty) in the given \
                     scope, visible at the current conversation's trust level."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "scope":  {"type": "string"},
                        "prefix": {"type": "string", "default": ""}
                    },
                    "required": ["scope"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::MemoryRead],
                default_allowed_classes: default_allowed_for_memory(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ListMemoryTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ListMemoryArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let memory = match ctx.memory.as_ref() {
            Some(m) => m,
            None => {
                return ToolOutcome::denied(
                    "memory capability not granted to this tool",
                );
            }
        };
        match memory.list(&args.scope, &args.prefix).await {
            Ok(entries) => ToolOutcome::Ok(json!({
                "keys": entries.iter().map(|e| json!({
                    "key": e.key,
                    "updated_at": e.updated_at,
                })).collect::<Vec<_>>(),
                "note": "list_memory scan not yet implemented; use read_memory for known keys"
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// set_thread_name
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SetThreadNameArgs {
    name: String,
}

pub struct SetThreadNameTool {
    descriptor: ToolDescriptor,
}

impl Default for SetThreadNameTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SetThreadNameTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "set_thread_name".into(),
                description:
                    "Set the human-readable title for the CURRENT thread. Use a concise 3-word \
                     summary that reflects the topic. Call this once enough context has \
                     accumulated; you can call it again later to refine."
                        .into(),
                schema: json!({
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
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ConversationWrite],
                default_allowed_classes: default_allowed_for_conversation_write(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SetThreadNameTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: SetThreadNameArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let conv = match ctx.conversation.as_ref() {
            Some(c) => c,
            None => {
                return ToolOutcome::denied(
                    "conversation capability not granted to this tool",
                );
            }
        };
        match conv.set_thread_name(&args.name).await {
            Ok(()) => ToolOutcome::Ok(json!({"ok": true, "name": args.name.trim()})),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// read_chat_history
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReadChatHistoryArgs {
    /// Optional pagination cursor — return events with `seq <
    /// before_seq`. Omit (or pass null) for the newest window.
    #[serde(default)]
    before_seq: Option<i64>,
    /// Max events to return. Capped at 200 server-side; sub-1 values
    /// are bumped to 1.
    #[serde(default = "default_history_limit")]
    limit: u32,
}

fn default_history_limit() -> u32 {
    20
}

pub struct ReadChatHistoryTool {
    descriptor: ToolDescriptor,
}

impl Default for ReadChatHistoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadChatHistoryTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "read_chat_history".into(),
                description:
                    "Read recent user / agent messages from the CURRENT thread, newest first. \
                     Returns up to `limit` entries; paginate older history with `before_seq`. \
                     Internal events (alerts, voice frames, phase markers) are filtered out — \
                     only the actual conversation transcript is returned."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "before_seq": {
                            "type": ["integer", "null"],
                            "description": "Optional. Return events with seq < before_seq. \
                                            Omit for the newest window."
                        },
                        "limit": {
                            "type": "integer",
                            "default": 20,
                            "minimum": 1,
                            "maximum": 200,
                            "description": "Max events to return. Server caps at 200."
                        }
                    },
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ConversationRead],
                default_allowed_classes: default_allowed_for_conversation_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ReadChatHistoryTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ReadChatHistoryArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let conv = match ctx.conversation.as_ref() {
            Some(c) => c,
            None => {
                return ToolOutcome::denied(
                    "conversation capability not granted to this tool",
                );
            }
        };
        match conv.read_history(args.before_seq, args.limit).await {
            Ok(entries) => ToolOutcome::Ok(json!({
                "entries": entries.iter().map(|e| json!({
                    "seq": e.seq,
                    "role": e.role,
                    "text": e.text,
                    "committed_at": e.committed_at,
                })).collect::<Vec<_>>(),
                "count": entries.len(),
                "next_before_seq": entries.last().map(|e| e.seq),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// get_thread
// ---------------------------------------------------------------

pub struct GetThreadTool {
    descriptor: ToolDescriptor,
}

impl Default for GetThreadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GetThreadTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "get_thread".into(),
                description:
                    "Return metadata about the CURRENT thread (conversation id, current display \
                     name). Use this to confirm orientation before calling other thread tools."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ConversationRead],
                default_allowed_classes: default_allowed_for_conversation_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for GetThreadTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let conv = match ctx.conversation.as_ref() {
            Some(c) => c,
            None => {
                return ToolOutcome::denied(
                    "conversation capability not granted to this tool",
                );
            }
        };
        match conv.get_thread().await {
            Ok(info) => ToolOutcome::Ok(json!({
                "conversation_id": info.conversation_id,
                "display_name": info.display_name,
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// notify_controller
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NotifyControllerArgs {
    title: String,
    #[serde(default)]
    detail: Option<String>,
    /// Optional severity. Defaults to `Info` if omitted.
    #[serde(default)]
    severity: Option<String>,
}

fn default_allowed_for_notify() -> Vec<String> {
    // Notifications are how an agent reaches the operator —
    // any active conversation legitimately wants this. The dedup
    // path in `DbNotifyApi` keeps a misbehaving agent from
    // drowning the controller.
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
        "UnknownPending".into(),
    ]
}

pub struct NotifyControllerTool {
    descriptor: ToolDescriptor,
}

impl Default for NotifyControllerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl NotifyControllerTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "notify_controller".into(),
                description:
                    "Send a notification to the controller. Routes through the operator's \
                     configured alert surface (UI dropdown by default; Signal fallback \
                     when present). Use this when you need the operator's attention — not for \
                     normal conversational replies. Duplicate notifications dedup against \
                     a single firing alert."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Short headline (\u{2264} 200 chars)."
                        },
                        "detail": {
                            "type": ["string", "null"],
                            "description": "Optional longer-form explanation (\u{2264} 4000 chars)."
                        },
                        "severity": {
                            "type": "string",
                            "enum": ["Info", "Warning", "Error", "Critical"],
                            "default": "Info",
                            "description": "Severity hint. Defaults to Info."
                        }
                    },
                    "required": ["title"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::Notify],
                default_allowed_classes: default_allowed_for_notify(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for NotifyControllerTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: NotifyControllerArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let notify = match ctx.notify.as_ref() {
            Some(n) => n,
            None => {
                return ToolOutcome::denied(
                    "notify capability not granted to this tool",
                );
            }
        };
        let severity = match args.severity.as_deref() {
            None => NotifySeverity::Info,
            Some(s) => match NotifySeverity::parse(s) {
                Some(v) => v,
                None => {
                    return ToolOutcome::err(
                        "invalid_argument",
                        format!("unknown severity {s:?}; expected Info/Warning/Error/Critical"),
                    );
                }
            },
        };
        match notify
            .notify(severity, &args.title, args.detail.as_deref())
            .await
        {
            Ok(receipt) => ToolOutcome::Ok(json!({
                "alert_id": receipt.alert_id,
                "deduplicated": receipt.deduplicated,
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// Registrar
// ---------------------------------------------------------------

/// Returns every core built-in as a registry-ready `Arc<dyn
/// ToolImpl>`. The host calls this once at boot to populate the
/// `HookRegistry`'s built-in tier and to seed `config_tool_access`
/// rows from each descriptor's `default_allowed_classes`.
pub fn core_builtin_tools() -> Vec<Arc<dyn ToolImpl>> {
    vec![
        Arc::new(ReadMemoryTool::new()),
        Arc::new(WriteMemoryTool::new()),
        Arc::new(ListMemoryTool::new()),
        Arc::new(SetThreadNameTool::new()),
        Arc::new(GetThreadTool::new()),
        Arc::new(ReadChatHistoryTool::new()),
        Arc::new(NotifyControllerTool::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use crate::db::{Database, DbConfig};
    use crate::ids::{ConversationId, EventSeq};
    use crate::migrations::MigrationRunner;
    use crate::tool::{Clock, MemoryApi, SystemClock};
    use crate::tool_apis::{DbConversationApi, DbMemoryApi, DbNotifyApi};

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

    fn build_ctx(
        db: &Database,
        cid: ConversationId,
        trust: &str,
        with_conv: bool,
        with_mem: bool,
    ) -> ToolCtx {
        build_ctx_with(db, cid, trust, with_conv, with_mem, false)
    }

    fn build_ctx_with(
        db: &Database,
        cid: ConversationId,
        trust: &str,
        with_conv: bool,
        with_mem: bool,
        with_notify: bool,
    ) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid.clone(), trust, clock.clone());
        if with_conv {
            ctx.conversation = Some(Arc::new(DbConversationApi::new(
                db.clone(),
                cid.clone(),
            )));
        }
        if with_mem {
            ctx.memory = Some(Arc::new(DbMemoryApi::new(
                db.clone(),
                trust,
                clock.now_unix(),
            )));
        }
        if with_notify {
            ctx.notify = Some(Arc::new(DbNotifyApi::new(
                db.clone(),
                cid,
                clock.now_unix(),
            )));
        }
        ctx
    }

    // --- Registrar ---

    #[test]
    fn core_builtin_tools_returns_every_expected_tool() {
        let tools = core_builtin_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.descriptor().name.as_str()).collect();
        assert!(names.contains(&"read_memory"));
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"list_memory"));
        assert!(names.contains(&"set_thread_name"));
        assert!(names.contains(&"get_thread"));
        assert!(names.contains(&"read_chat_history"));
        assert!(names.contains(&"notify_controller"));
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn core_builtin_tools_descriptors_declare_required_capabilities() {
        let by_name: std::collections::HashMap<String, Arc<dyn ToolImpl>> =
            core_builtin_tools()
                .into_iter()
                .map(|t| (t.descriptor().name.clone(), t))
                .collect();
        assert_eq!(
            by_name["read_memory"].descriptor().capabilities,
            vec![Capability::MemoryRead]
        );
        assert_eq!(
            by_name["write_memory"].descriptor().capabilities,
            vec![Capability::MemoryWrite]
        );
        assert_eq!(
            by_name["set_thread_name"].descriptor().capabilities,
            vec![Capability::ConversationWrite]
        );
        assert_eq!(
            by_name["get_thread"].descriptor().capabilities,
            vec![Capability::ConversationRead]
        );
    }

    #[test]
    fn core_builtin_tools_all_tagged_as_builtin_source() {
        for tool in core_builtin_tools() {
            assert_eq!(tool.descriptor().source, ToolSource::Builtin);
        }
    }

    /// Critical security invariant: every tool's
    /// `default_allowed_classes` must NOT include `Blocked`. A Blocked
    /// principal calling any tool is a revocation we don't want to
    /// undo by accident in a future descriptor edit.
    #[test]
    fn no_default_allowlist_includes_blocked() {
        for tool in core_builtin_tools() {
            assert!(
                !tool.descriptor()
                    .default_allowed_classes
                    .iter()
                    .any(|c| c == "Blocked"),
                "tool '{}' allows Blocked by default — security regression",
                tool.descriptor().name
            );
        }
    }

    // --- read_memory ---

    #[tokio::test]
    async fn read_memory_returns_stored_value() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        // Pre-populate via the API directly.
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("s", "k", "hello")
            .await
            .unwrap();
        let tool = ReadMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let out = tool
            .invoke(ctx, json!({"scope": "s", "key": "k"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => assert_eq!(v, json!("hello")),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_memory_returns_null_when_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c2");
        let tool = ReadMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let out = tool
            .invoke(ctx, json!({"scope": "s", "key": "missing"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => assert_eq!(v, Value::Null),
            other => panic!("expected Ok(null), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_memory_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c3");
        let tool = ReadMemoryTool::new();
        // Memory cap intentionally not populated.
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match tool.invoke(ctx, json!({"scope": "s", "key": "k"})).await {
            ToolOutcome::Denied { reason } => {
                assert!(reason.contains("memory"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_memory_invalid_args_returns_err() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c4");
        let tool = ReadMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        match tool.invoke(ctx, json!({"key_only": "x"})).await {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// Adversarial: a low-trust caller cannot read controller memory
    /// even by addressing it directly.
    #[tokio::test]
    async fn read_memory_low_trust_cannot_read_controller_value() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c5");
        DbMemoryApi::new(db.clone(), "Controller", 0)
            .write("global", "secret", "top-secret")
            .await
            .unwrap();
        let tool = ReadMemoryTool::new();
        let ctx = build_ctx(&db, cid, "UnknownPending", false, true);
        match tool.invoke(ctx, json!({"scope": "global", "key": "secret"})).await {
            ToolOutcome::Ok(v) => assert_eq!(v, Value::Null),
            other => panic!("expected null, got {other:?}"),
        }
    }

    // --- write_memory ---

    #[tokio::test]
    async fn write_memory_succeeds_and_persists() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c6");
        let tool = WriteMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let out = tool
            .invoke(ctx, json!({"scope": "s", "key": "k", "value": "v"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => assert_eq!(v["ok"], true),
            other => panic!("expected Ok, got {other:?}"),
        }
        // Verify via underlying store.
        let stored = crate::memory::MemoryStore::new(&db)
            .get("s", "Controller", "k")
            .unwrap();
        assert!(stored.is_some());
    }

    /// Adversarial: an LLM tries to escalate by passing `trust_class`
    /// in the args. The `WriteMemoryArgs` deserializer ignores extras
    /// (serde default), and the capability impl always uses the
    /// caller-bound trust class regardless of args.
    #[tokio::test]
    async fn write_memory_ignores_llm_supplied_trust_class() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c7");
        let tool = WriteMemoryTool::new();
        let ctx = build_ctx(&db, cid, "KnownLimited", false, true);
        tool.invoke(
            ctx,
            json!({
                "scope": "s",
                "key": "k",
                "value": "v",
                "trust_class": "Controller"
            }),
        )
        .await;
        // The row must be at KnownLimited, not Controller.
        let store = crate::memory::MemoryStore::new(&db);
        assert!(store.get("s", "KnownLimited", "k").unwrap().is_some());
        assert!(store.get("s", "Controller", "k").unwrap().is_none());
    }

    #[tokio::test]
    async fn write_memory_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c8");
        let tool = WriteMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match tool
            .invoke(ctx, json!({"scope": "s", "key": "k", "value": "v"}))
            .await
        {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- list_memory ---

    #[tokio::test]
    async fn list_memory_returns_stub_shape() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c9");
        let tool = ListMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let out = tool.invoke(ctx, json!({"scope": "s"})).await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["keys"], json!([]));
                assert!(v["note"].as_str().unwrap().contains("not yet implemented"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    // --- set_thread_name ---

    #[tokio::test]
    async fn set_thread_name_writes_display_name_through() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c10");
        let tool = SetThreadNameTool::new();
        let ctx = build_ctx(&db, cid.clone(), "Controller", true, false);
        let out = tool
            .invoke(ctx, json!({"name": "Q4 budget review"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["ok"], true);
                assert_eq!(v["name"], "Q4 budget review");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        let row = ConversationStore::new(&db).get(&cid).unwrap().unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Q4 budget review"));
    }

    #[tokio::test]
    async fn set_thread_name_validates_empty_input() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c11");
        let tool = SetThreadNameTool::new();
        let ctx = build_ctx(&db, cid, "Controller", true, false);
        match tool.invoke(ctx, json!({"name": "   "})).await {
            ToolOutcome::Err { code, .. } => {
                assert_eq!(code, "invalid_argument");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_thread_name_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c12");
        let tool = SetThreadNameTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match tool.invoke(ctx, json!({"name": "x"})).await {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- get_thread ---

    #[tokio::test]
    async fn get_thread_returns_metadata() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c13");
        ConversationStore::new(&db)
            .set_display_name(&cid, Some("My Topic"))
            .unwrap();
        let tool = GetThreadTool::new();
        let ctx = build_ctx(&db, cid, "Controller", true, false);
        let out = tool.invoke(ctx, json!({})).await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["conversation_id"], "c13");
                assert_eq!(v["display_name"], "My Topic");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_thread_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c14");
        let tool = GetThreadTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match tool.invoke(ctx, json!({})).await {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- read_chat_history --------------------------------------------

    use crate::events::{EventKind, EventLog, EventRecord};

    fn append_user_event(db: &Database, cid: &ConversationId, seq: i64, text: &str) {
        let ev = EventRecord::new(
            cid.clone(),
            EventSeq(seq),
            EventKind::UserMsg,
            &serde_json::json!({"text": text}),
            Some("controller".into()),
        )
        .unwrap();
        EventLog::new(db).append(&ev).unwrap();
    }

    #[tokio::test]
    async fn read_chat_history_returns_entries_with_pagination_cursor() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "h1");
        for (seq, text) in [(1, "first"), (2, "second"), (3, "third")] {
            append_user_event(&db, &cid, seq, text);
        }
        let tool = ReadChatHistoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", true, false);
        let out = tool.invoke(ctx, json!({"limit": 10})).await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["count"], 3);
                let entries = v["entries"].as_array().unwrap();
                assert_eq!(entries.len(), 3);
                // Newest first.
                assert_eq!(entries[0]["text"], "third");
                assert_eq!(entries[0]["role"], "user");
                // Cursor for next page is the oldest seq in this window.
                assert_eq!(v["next_before_seq"], 1);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_chat_history_default_limit_when_omitted() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "h2");
        for i in 1..=25 {
            append_user_event(&db, &cid, i, "x");
        }
        let tool = ReadChatHistoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", true, false);
        let out = tool.invoke(ctx, json!({})).await;
        match out {
            ToolOutcome::Ok(v) => {
                // Default limit is 20.
                assert_eq!(v["count"], 20);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_chat_history_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "h3");
        let tool = ReadChatHistoryTool::new();
        // Conversation cap intentionally not populated.
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match tool.invoke(ctx, json!({})).await {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_chat_history_invalid_args_returns_err() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "h4");
        let tool = ReadChatHistoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", true, false);
        match tool.invoke(ctx, json!({"before_seq": "not-a-number"})).await {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    // --- notify_controller --------------------------------------------

    use crate::alerts::AlertStore;

    #[tokio::test]
    async fn notify_controller_inserts_firing_alert() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "n1");
        let tool = NotifyControllerTool::new();
        let ctx = build_ctx_with(&db, cid, "Controller", false, false, true);
        let out = tool
            .invoke(
                ctx,
                json!({"title": "Build failed", "detail": "exit 1", "severity": "Error"}),
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert!(v["alert_id"].is_string());
                assert_eq!(v["deduplicated"], false);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(AlertStore::new(&db).count_firing().unwrap(), 1);
    }

    /// Default severity when omitted is `Info`.
    #[tokio::test]
    async fn notify_controller_defaults_to_info_severity() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "n2");
        let tool = NotifyControllerTool::new();
        let ctx = build_ctx_with(&db, cid, "Controller", false, false, true);
        tool.invoke(ctx, json!({"title": "fyi"})).await;
        // Inspect the inserted row.
        let rows = AlertStore::new(&db)
            .list(Some(&[crate::alerts::AlertStatus::Firing]), Some(100))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, crate::alerts::Severity::Info);
    }

    /// Repeat calls dedup against the existing firing alert and the
    /// receipt's `deduplicated` flag flips on the second call.
    #[tokio::test]
    async fn notify_controller_deduplicates_repeated_call() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "n3");
        let tool = NotifyControllerTool::new();
        let ctx1 = build_ctx_with(&db, cid.clone(), "Controller", false, false, true);
        let r1 = tool
            .invoke(ctx1, json!({"title": "Same", "severity": "Warning"}))
            .await;
        match r1 {
            ToolOutcome::Ok(v) => assert_eq!(v["deduplicated"], false),
            other => panic!("expected Ok, got {other:?}"),
        }
        let ctx2 = build_ctx_with(&db, cid, "Controller", false, false, true);
        let r2 = tool
            .invoke(ctx2, json!({"title": "Same", "severity": "Warning"}))
            .await;
        match r2 {
            ToolOutcome::Ok(v) => assert_eq!(v["deduplicated"], true),
            other => panic!("expected Ok, got {other:?}"),
        }
        // Still exactly one firing alert.
        assert_eq!(AlertStore::new(&db).count_firing().unwrap(), 1);
    }

    #[tokio::test]
    async fn notify_controller_validates_empty_title() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "n4");
        let tool = NotifyControllerTool::new();
        let ctx = build_ctx_with(&db, cid, "Controller", false, false, true);
        match tool.invoke(ctx, json!({"title": "  "})).await {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_controller_rejects_unknown_severity() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "n5");
        let tool = NotifyControllerTool::new();
        let ctx = build_ctx_with(&db, cid, "Controller", false, false, true);
        match tool
            .invoke(ctx, json!({"title": "x", "severity": "Catastrophic"}))
            .await
        {
            ToolOutcome::Err { code, message } => {
                assert_eq!(code, "invalid_argument");
                assert!(message.contains("Catastrophic"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_controller_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "n6");
        let tool = NotifyControllerTool::new();
        // No `with_notify` cap.
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match tool.invoke(ctx, json!({"title": "x"})).await {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }
}
