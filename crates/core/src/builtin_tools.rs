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
    Capability, NotifySeverity, RoutineSummary, SubagentRequest, ToolCtx, ToolDescriptor, ToolImpl,
    ToolLatency, ToolOutcome, ToolSource,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value as JsonValue;
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
                return ToolOutcome::denied("memory capability not granted to this tool");
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
                description: "Write a memory value at the current conversation's trust scope. \
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
                return ToolOutcome::denied("memory capability not granted to this tool");
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
                return ToolOutcome::denied("memory capability not granted to this tool");
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
                return ToolOutcome::denied("conversation capability not granted to this tool");
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
                return ToolOutcome::denied("conversation capability not granted to this tool");
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
// list_chats
// ---------------------------------------------------------------

pub struct ListChatsTool {
    descriptor: ToolDescriptor,
}

impl Default for ListChatsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListChatsTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "list_chats".into(),
                description:
                    "List every non-ephemeral conversation thread visible to the caller, sorted \
                     newest-first by last activity. Returns id, display name, trust class, \
                     pinned flag, and last_activity_at. Use this to find a thread by name \
                     before calling per-thread tools."
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
impl ToolImpl for ListChatsTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let conv = match ctx.conversation.as_ref() {
            Some(c) => c,
            None => {
                return ToolOutcome::denied("conversation capability not granted to this tool");
            }
        };
        match conv.list_threads().await {
            Ok(rows) => ToolOutcome::Ok(json!({
                "threads": rows.iter().map(|t| json!({
                    "conversation_id": t.conversation_id,
                    "display_name": t.display_name,
                    "trust_class": t.trust_class,
                    "is_pinned": t.is_pinned,
                    "last_activity_at": t.last_activity_at,
                })).collect::<Vec<_>>(),
                "count": rows.len(),
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
                return ToolOutcome::denied("conversation capability not granted to this tool");
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
                return ToolOutcome::denied("notify capability not granted to this tool");
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
// schedule_task family — wraps RoutineStore via ScheduleApi.
// ---------------------------------------------------------------

fn default_allowed_for_schedule_read() -> Vec<String> {
    // Routines surface operator-visible automation state — every
    // listed routine reveals the operator's prompt + target
    // conversation. Controller only by default; operator can
    // broaden via Settings → Tools if a Delegated workflow needs
    // to read its own routines.
    vec!["Controller".into()]
}

fn default_allowed_for_schedule_write() -> Vec<String> {
    // Routines mutate operator-visible automation state and can fire
    // prompts as the controller into any conversation — Controller
    // only by default. Operator can broaden via the Settings → Tools
    // page if a particular workflow needs Delegated/etc to manage
    // their own routines.
    vec!["Controller".into()]
}

fn summary_to_json(s: &RoutineSummary) -> Value {
    json!({
        "id": s.id,
        "name": s.name,
        "schedule_cron": s.schedule_cron,
        "timezone": s.timezone,
        "prompt": s.prompt,
        "target_conversation_id": s.target_conversation_id,
        "enabled": s.enabled,
        "last_run_at": s.last_run_at,
        "last_run_status": s.last_run_status,
        "next_run_at": s.next_run_at,
    })
}

#[derive(Debug, Deserialize)]
struct ScheduleTaskArgs {
    name: String,
    schedule_cron: String,
    prompt: String,
    #[serde(default)]
    target_conversation_id: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

pub struct CreateRoutineTool {
    descriptor: ToolDescriptor,
}

impl Default for CreateRoutineTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CreateRoutineTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_create".into(),
                description:
                    "Create a recurring routine. The cron expression is in standard 5-field form \
                     (minute hour day-of-month month day-of-week). The routine fires `prompt` \
                     into the target conversation (caller's thread by default) on each schedule \
                     tick. Returns the new routine's id."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Human-readable label."},
                        "schedule_cron": {
                            "type": "string",
                            "description": "5-field cron, e.g. '0 9 * * MON-FRI'."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Prompt that fires into the target conversation."
                        },
                        "target_conversation_id": {
                            "type": ["string", "null"],
                            "description": "Optional. Defaults to the caller's own thread. \
                                            Only Controller can target another thread."
                        },
                        "timezone": {
                            "type": "string",
                            "default": "UTC",
                            "description": "IANA timezone, e.g. 'America/Los_Angeles'."
                        }
                    },
                    "required": ["name", "schedule_cron", "prompt"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleWrite],
                default_allowed_classes: default_allowed_for_schedule_write(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for CreateRoutineTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ScheduleTaskArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched
            .create_routine(
                &args.name,
                &args.schedule_cron,
                &args.prompt,
                args.target_conversation_id.as_deref(),
                args.timezone.as_deref(),
            )
            .await
        {
            Ok(s) => ToolOutcome::Ok(summary_to_json(&s)),
            Err(e) => e.into_outcome(),
        }
    }
}

pub struct ListRoutinesTool {
    descriptor: ToolDescriptor,
}

impl Default for ListRoutinesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListRoutinesTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_list".into(),
                description: "List every recurring routine currently registered. Returns id, \
                     name, cron, target, enabled flag, and last/next-run timestamps."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleRead],
                default_allowed_classes: default_allowed_for_schedule_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ListRoutinesTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched.list_routines().await {
            Ok(rows) => ToolOutcome::Ok(json!({
                "routines": rows.iter().map(summary_to_json).collect::<Vec<_>>(),
                "count": rows.len(),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RoutineIdArgs {
    routine_id: String,
}

pub struct GetRoutineTool {
    descriptor: ToolDescriptor,
}

impl Default for GetRoutineTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GetRoutineTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_get".into(),
                description:
                    "Look up a single routine by id. Returns the full row (name, cron, target, \
                     enabled, last/next run timestamps) or null if no routine matches."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {"routine_id": {"type": "string"}},
                    "required": ["routine_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleRead],
                default_allowed_classes: default_allowed_for_schedule_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for GetRoutineTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: RoutineIdArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched.get_routine(&args.routine_id).await {
            Ok(Some(s)) => ToolOutcome::Ok(summary_to_json(&s)),
            Ok(None) => ToolOutcome::Ok(json!(null)),
            Err(e) => e.into_outcome(),
        }
    }
}

pub struct DeleteRoutineTool {
    descriptor: ToolDescriptor,
}

impl Default for DeleteRoutineTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DeleteRoutineTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_delete".into(),
                description:
                    "Permanently delete a routine. Returns `{deleted: true}` on success, \
                     `{deleted: false}` if no routine matched."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "routine_id": {"type": "string"}
                    },
                    "required": ["routine_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleWrite],
                default_allowed_classes: default_allowed_for_schedule_write(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for DeleteRoutineTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: RoutineIdArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched.delete_routine(&args.routine_id).await {
            Ok(deleted) => ToolOutcome::Ok(json!({"deleted": deleted})),
            Err(e) => e.into_outcome(),
        }
    }
}

pub struct PauseRoutineTool {
    descriptor: ToolDescriptor,
}
impl Default for PauseRoutineTool {
    fn default() -> Self {
        Self::new()
    }
}
impl PauseRoutineTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_pause".into(),
                description: "Pause a routine without deleting it. The routine stops firing \
                              until `routine_resume` re-enables it."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {"routine_id": {"type": "string"}},
                    "required": ["routine_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleWrite],
                default_allowed_classes: default_allowed_for_schedule_write(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for PauseRoutineTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: RoutineIdArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched.set_enabled(&args.routine_id, false).await {
            Ok(s) => ToolOutcome::Ok(summary_to_json(&s)),
            Err(e) => e.into_outcome(),
        }
    }
}

pub struct ResumeRoutineTool {
    descriptor: ToolDescriptor,
}
impl Default for ResumeRoutineTool {
    fn default() -> Self {
        Self::new()
    }
}
impl ResumeRoutineTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_resume".into(),
                description: "Re-enable a paused routine.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {"routine_id": {"type": "string"}},
                    "required": ["routine_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleWrite],
                default_allowed_classes: default_allowed_for_schedule_write(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ResumeRoutineTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: RoutineIdArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched.set_enabled(&args.routine_id, true).await {
            Ok(s) => ToolOutcome::Ok(summary_to_json(&s)),
            Err(e) => e.into_outcome(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpdateRoutineArgs {
    routine_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    schedule_cron: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    target_conversation_id: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

pub struct UpdateRoutineTool {
    descriptor: ToolDescriptor,
}
impl Default for UpdateRoutineTool {
    fn default() -> Self {
        Self::new()
    }
}
impl UpdateRoutineTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "routine_update".into(),
                description: "Update a routine. Pass only the fields you want to change; \
                              omitted fields stay at their current value."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "routine_id": {"type": "string"},
                        "name": {"type": ["string", "null"]},
                        "schedule_cron": {"type": ["string", "null"]},
                        "prompt": {"type": ["string", "null"]},
                        "target_conversation_id": {"type": ["string", "null"]},
                        "enabled": {"type": ["boolean", "null"]}
                    },
                    "required": ["routine_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ScheduleWrite],
                default_allowed_classes: default_allowed_for_schedule_write(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for UpdateRoutineTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: UpdateRoutineArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let sched = match ctx.schedule.as_ref() {
            Some(s) => s,
            None => {
                return ToolOutcome::denied("schedule capability not granted to this tool");
            }
        };
        match sched
            .update_routine(
                &args.routine_id,
                args.name.as_deref(),
                args.schedule_cron.as_deref(),
                args.prompt.as_deref(),
                args.target_conversation_id.as_deref(),
                args.enabled,
            )
            .await
        {
            Ok(s) => ToolOutcome::Ok(summary_to_json(&s)),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// web_fetch — HTTP GET against the wider internet, SSRF-guarded.
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    /// Truncate the returned body to this many characters before
    /// handing it to the model. Default 3000 (~750 tokens) — keeps
    /// a typical fetched-page from blowing the context window
    /// across multiple tool rounds. The HTTP-layer cap of 1 MiB
    /// still applies; this is the *agent-visible* cap on top.
    /// 2026-05-02 — pre-fix the agent would consume entire pages
    /// (50-200KB ≈ 12-50K tokens), making 3-round web_search →
    /// web_fetch → synthesise turns hit the model's context.
    #[serde(default = "default_web_fetch_max_chars")]
    max_chars: usize,
}

fn default_web_fetch_max_chars() -> usize {
    3000
}

fn default_allowed_for_web_fetch() -> Vec<String> {
    // Outbound HTTP touches the wider internet. Trust-class scoping
    // here mirrors `read_chat_history` — Controller / Delegated /
    // KnownTrusted / KnownLimited can call it; cold callers
    // (`UnknownPending`) cannot. The implementation's SSRF guard +
    // size cap + content-type allowlist provide the additional
    // belt-and-braces.
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
    ]
}

pub struct WebFetchTool {
    descriptor: ToolDescriptor,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "web_fetch".into(),
                description:
                    "Fetch a URL via HTTP GET and return the response body as text. Limited to \
                     http(s); private/loopback/link-local addresses are rejected; binary \
                     content types are rejected. The agent-visible body is capped at 3000 chars \
                     by default (override with `max_chars`, up to 50000) to keep multi-step \
                     research flows from blowing the model's context window. Useful for reading \
                     articles, JSON APIs, RSS feeds, and other public textual content."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "format": "uri",
                            "description": "Absolute http or https URL to fetch."
                        },
                        "max_chars": {
                            "type": "integer",
                            "default": 3000,
                            "minimum": 256,
                            "maximum": 50000,
                            "description": "Cap on the returned body length (chars). Default 3000 (~750 tokens). Bump if the page is long and you need more — the response sets `truncated: true` when this fires."
                        }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::WebFetch],
                default_allowed_classes: default_allowed_for_web_fetch(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for WebFetchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: WebFetchArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.web_fetch.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("web_fetch capability not granted to this tool");
            }
        };
        // Cap the agent-visible body at `max_chars`. We slice on
        // char boundaries — naive byte slicing would corrupt UTF-8
        // mid-codepoint and the JSON serialisation downstream
        // would error.
        let cap = args.max_chars.clamp(256, 50_000);
        match api.get(&args.url).await {
            Ok(resp) => {
                let (body, truncated_by_agent_cap) = if resp.body.chars().count() > cap {
                    let mut s = String::with_capacity(cap + 8);
                    for ch in resp.body.chars().take(cap) {
                        s.push(ch);
                    }
                    s.push_str("\n…");
                    (s, true)
                } else {
                    (resp.body, false)
                };
                ToolOutcome::Ok(json!({
                    "final_url": resp.final_url,
                    "status": resp.status,
                    "content_type": resp.content_type,
                    "body": body,
                    // `truncated` is true if EITHER the HTTP-layer
                    // cap (1 MiB) OR the agent-visible cap fired.
                    // The agent doesn't need to distinguish — both
                    // mean "request more via max_chars to see more".
                    "truncated": resp.truncated || truncated_by_agent_cap,
                }))
            }
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// web_search — provider-pluggable; default DuckDuckGo (no API key).
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_search_max_results")]
    max_results: u32,
}

fn default_search_max_results() -> u32 {
    8
}

fn default_allowed_for_search() -> Vec<String> {
    // Same allowlist semantic as web_fetch — search reaches the wider
    // internet via whichever provider the operator chose.
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
    ]
}

pub struct WebSearchTool {
    descriptor: ToolDescriptor,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "web_search".into(),
                description:
                    "Search the public web. Routes through the operator's configured search \
                     provider (DuckDuckGo by default; Brave / Exa / Tavily / Kagi / SearxNG \
                     selectable in Settings). Returns up to `max_results` items as \
                     `[{title, url, snippet}]`."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query."
                        },
                        "max_results": {
                            "type": "integer",
                            "default": 8,
                            "minimum": 1,
                            "maximum": 25
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::Search],
                default_allowed_classes: default_allowed_for_search(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for WebSearchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: WebSearchArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        if args.query.trim().is_empty() {
            return ToolOutcome::err("invalid_argument", "query is empty");
        }
        let api = match ctx.search.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("search capability not granted to this tool");
            }
        };
        let provider = api.provider_id().to_owned();
        match api.search(&args.query, args.max_results.clamp(1, 25)).await {
            Ok(results) => ToolOutcome::Ok(json!({
                "provider": provider,
                "results": results.iter().map(|r| json!({
                    "title": r.title,
                    "url": r.url,
                    "snippet": r.snippet,
                })).collect::<Vec<_>>(),
                "count": results.len(),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// delegate_task — synchronous subagent call (child LLM turn).
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DelegateTaskArgs {
    task: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

fn default_allowed_for_subagent_spawn() -> Vec<String> {
    // Subagent spawning is a model-loop multiplier — allow only
    // trusted callers. Operator can broaden in Settings → Tools if
    // a workflow needs it.
    vec!["Controller".into(), "Delegated".into()]
}

pub struct DelegateTaskTool {
    descriptor: ToolDescriptor,
}

impl Default for DelegateTaskTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DelegateTaskTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "delegate_task".into(),
                description:
                    "Spawn a subagent (child LLM call) for a focused sub-task. The parent's \
                     turn pauses until the subagent returns its text reply. Use this to \
                     delegate work that benefits from context isolation — drafting, summarising \
                     a long excerpt, formatting structured output. For multi-minute background \
                     work use the research tools instead."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "What the subagent should do (the prompt)."
                        },
                        "context": {
                            "type": ["string", "null"],
                            "description": "Optional context attached verbatim ahead of the task."
                        },
                        "max_tokens": {
                            "type": ["integer", "null"],
                            "minimum": 1,
                            "maximum": 4096,
                            "description": "Cap on the subagent's reply length."
                        }
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::High,
                capabilities: vec![Capability::SubagentSpawn],
                default_allowed_classes: default_allowed_for_subagent_spawn(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for DelegateTaskTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: DelegateTaskArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        if args.task.trim().is_empty() {
            return ToolOutcome::err("invalid_argument", "task is empty");
        }
        let api = match ctx.subagent.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("subagent capability not granted to this tool");
            }
        };
        let req = SubagentRequest {
            task: args.task,
            context: args.context,
            max_tokens: args.max_tokens.map(|n| n.min(4096)),
        };
        match api.delegate(&req).await {
            Ok(resp) => ToolOutcome::Ok(json!({
                "task_id": resp.task_id,
                "text": resp.text,
                "tokens_used": resp.tokens_used,
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// research_start / research_status / research_list
// ---------------------------------------------------------------

fn default_allowed_for_research_spawn() -> Vec<String> {
    // Spawning a deep-research job is a meaningful resource burn —
    // keep it Controller + Delegated by default. Operators can
    // broaden in Settings → Tools.
    vec!["Controller".into(), "Delegated".into()]
}

fn default_allowed_for_research_read() -> Vec<String> {
    // Reading job status is harmless; allow every active class.
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
        "UnknownPending".into(),
    ]
}

fn job_view_to_json(v: &crate::tool::ResearchJobView) -> JsonValue {
    json!({
        "id": v.id,
        "conversation_id": v.conversation_id,
        "query": v.query,
        "status": v.status,
        "card_id": v.card_id,
        "workspace_path": v.workspace_path,
        "attachment_id": v.attachment_id,
        "error": v.error,
        "created_at": v.created_at,
        "updated_at": v.updated_at,
        "started_at": v.started_at,
        "finished_at": v.finished_at,
        "plan": v.plan,
    })
}

#[derive(Debug, Deserialize)]
struct ResearchStartArgs {
    query: String,
    /// Optional per-job overrides on the global config_research
    /// defaults. JSON object; the runner reads it and clamps each
    /// override to the operator's ceiling at start time.
    #[serde(default)]
    overrides: Option<JsonValue>,
}

pub struct ResearchStartTool {
    descriptor: ToolDescriptor,
}

impl Default for ResearchStartTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchStartTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "research_start".into(),
                description:
                    "Enqueue a deep-research job for a question. The job runs asynchronously \
                     (minutes to hours) — this tool returns immediately with a job_id the \
                     operator can watch, and `research_status(job_id)` lets the agent poll \
                     for progress. For sub-minute focused work use `delegate_task` instead."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The research question — what should the runner investigate?"
                        },
                        "overrides": {
                            "type": ["object", "null"],
                            "description": "Optional per-job overrides on the operator's defaults. Keys mirror config_research."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ResearchSpawn],
                default_allowed_classes: default_allowed_for_research_spawn(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ResearchStartTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ResearchStartArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.research.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("research_spawn capability not granted to this tool");
            }
        };
        let overrides_blob = match args.overrides {
            Some(v) => match rmp_serde::to_vec(&v) {
                Ok(b) => Some(b),
                Err(e) => {
                    return ToolOutcome::err("invalid_argument", format!("encode overrides: {e}"));
                }
            },
            None => None,
        };
        match api.start(&args.query, overrides_blob).await {
            Ok(view) => ToolOutcome::Ok(json!({
                "job": job_view_to_json(&view),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResearchStatusArgs {
    job_id: String,
}

pub struct ResearchStatusTool {
    descriptor: ToolDescriptor,
}

impl Default for ResearchStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchStatusTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "research_status".into(),
                description:
                    "Poll a deep-research job's status. Returns the current row including \
                     the plan (if landed), the workspace path, the attachment id of the \
                     final report (if complete), and any error."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "The id returned by `research_start`."
                        }
                    },
                    "required": ["job_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ResearchRead],
                default_allowed_classes: default_allowed_for_research_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ResearchStatusTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ResearchStatusArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.research.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("research_read capability not granted to this tool");
            }
        };
        match api.status(&args.job_id).await {
            Ok(Some(view)) => ToolOutcome::Ok(json!({"job": job_view_to_json(&view)})),
            Ok(None) => ToolOutcome::err("not_found", format!("no job '{}' visible", args.job_id)),
            Err(e) => e.into_outcome(),
        }
    }
}

pub struct ResearchListTool {
    descriptor: ToolDescriptor,
}

impl Default for ResearchListTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchListTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "research_list".into(),
                description:
                    "List deep-research jobs visible to the caller. A Controller sees every job; \
                     other callers see only the jobs in their own conversation. Newest first."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ResearchRead],
                default_allowed_classes: default_allowed_for_research_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ResearchListTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let api = match ctx.research.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("research_read capability not granted to this tool");
            }
        };
        match api.list().await {
            Ok(views) => ToolOutcome::Ok(json!({
                "jobs": views.iter().map(job_view_to_json).collect::<Vec<_>>(),
                "count": views.len(),
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// research_get_report — fetch a completed job's synthesized markdown.
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ResearchGetReportArgs {
    job_id: String,
}

pub struct ResearchGetReportTool {
    descriptor: ToolDescriptor,
}

impl Default for ResearchGetReportTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchGetReportTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "research_get_report".into(),
                description:
                    "Fetch the synthesized markdown report for a completed deep-research job. \
                     Returns the report text or null if the job exists but has no report yet \
                     (still gathering / synthesizing / failed)."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "The id returned by `research_start`."
                        }
                    },
                    "required": ["job_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::ResearchRead],
                default_allowed_classes: default_allowed_for_research_read(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for ResearchGetReportTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ResearchGetReportArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.research.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("research_read capability not granted to this tool");
            }
        };
        match api.get_report(&args.job_id).await {
            Ok(Some(report)) => ToolOutcome::Ok(json!({
                "job_id": args.job_id,
                "report_markdown": report,
            })),
            Ok(None) => ToolOutcome::Ok(json!({
                "job_id": args.job_id,
                "report_markdown": Value::Null,
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
        Arc::new(ListChatsTool::new()),
        Arc::new(ReadChatHistoryTool::new()),
        Arc::new(NotifyControllerTool::new()),
        Arc::new(CreateRoutineTool::new()),
        Arc::new(ListRoutinesTool::new()),
        Arc::new(GetRoutineTool::new()),
        Arc::new(DeleteRoutineTool::new()),
        Arc::new(PauseRoutineTool::new()),
        Arc::new(ResumeRoutineTool::new()),
        Arc::new(UpdateRoutineTool::new()),
        Arc::new(WebFetchTool::new()),
        Arc::new(WebSearchTool::new()),
        Arc::new(DelegateTaskTool::new()),
        Arc::new(ResearchStartTool::new()),
        Arc::new(ResearchStatusTool::new()),
        Arc::new(ResearchListTool::new()),
        Arc::new(ResearchGetReportTool::new()),
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
    use crate::tool_apis::{DbConversationApi, DbMemoryApi, DbNotifyApi, DbScheduleApi};

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
        build_ctx_full(db, cid, trust, with_conv, with_mem, with_notify, false)
    }

    fn build_ctx_full(
        db: &Database,
        cid: ConversationId,
        trust: &str,
        with_conv: bool,
        with_mem: bool,
        with_notify: bool,
        with_schedule: bool,
    ) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid.clone(), trust, clock.clone());
        if with_conv {
            ctx.conversation = Some(Arc::new(DbConversationApi::new(db.clone(), cid.clone())));
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
                cid.clone(),
                clock.now_unix(),
            )));
        }
        if with_schedule {
            ctx.schedule = Some(Arc::new(DbScheduleApi::new(
                db.clone(),
                trust,
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
        assert!(names.contains(&"routine_create"));
        assert!(names.contains(&"routine_list"));
        assert!(names.contains(&"routine_get"));
        assert!(names.contains(&"routine_delete"));
        assert!(names.contains(&"routine_pause"));
        assert!(names.contains(&"routine_resume"));
        assert!(names.contains(&"routine_update"));
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"list_chats"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"delegate_task"));
        assert!(names.contains(&"research_start"));
        assert!(names.contains(&"research_status"));
        assert!(names.contains(&"research_list"));
        assert!(names.contains(&"research_get_report"));
        assert_eq!(names.len(), 22);
    }

    #[test]
    fn core_builtin_tools_descriptors_declare_required_capabilities() {
        let by_name: std::collections::HashMap<String, Arc<dyn ToolImpl>> = core_builtin_tools()
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
                !tool
                    .descriptor()
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
        let out = tool.invoke(ctx, json!({"scope": "s", "key": "k"})).await;
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
        match tool
            .invoke(ctx, json!({"scope": "global", "key": "secret"}))
            .await
        {
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
        let out = tool.invoke(ctx, json!({"name": "Q4 budget review"})).await;
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
        match tool
            .invoke(ctx, json!({"before_seq": "not-a-number"}))
            .await
        {
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

    // --- schedule_task family -----------------------------------------

    #[tokio::test]
    async fn schedule_task_creates_routine_with_required_fields() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s1");
        let tool = CreateRoutineTool::new();
        let ctx = build_ctx_full(&db, cid.clone(), "Controller", false, false, false, true);
        let out = tool
            .invoke(
                ctx,
                json!({
                    "name": "morning brief",
                    "schedule_cron": "0 9 * * *",
                    "prompt": "summarise overnight events"
                }),
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert!(v["id"].is_string());
                assert_eq!(v["name"], "morning brief");
                assert_eq!(v["enabled"], true);
                // Defaults to caller's conversation when target unset.
                assert_eq!(v["target_conversation_id"], cid.as_str());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn schedule_task_rejects_invalid_cron() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s2");
        let tool = CreateRoutineTool::new();
        let ctx = build_ctx_full(&db, cid, "Controller", false, false, false, true);
        let out = tool
            .invoke(
                ctx,
                json!({
                    "name": "bad",
                    "schedule_cron": "not actually cron",
                    "prompt": "p"
                }),
            )
            .await;
        match out {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// Adversarial: a non-Controller caller cannot target a different
    /// conversation than their own — the API rejects with NotAuthorized.
    #[tokio::test]
    async fn schedule_task_low_trust_cannot_target_other_conversation() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s3");
        let _other = seed_conversation(&db, "other-conv");
        let tool = CreateRoutineTool::new();
        let ctx = build_ctx_full(&db, cid, "KnownTrusted", false, false, false, true);
        let out = tool
            .invoke(
                ctx,
                json!({
                    "name": "x",
                    "schedule_cron": "0 9 * * *",
                    "prompt": "p",
                    "target_conversation_id": "other-conv"
                }),
            )
            .await;
        match out {
            ToolOutcome::Denied { reason } => {
                assert!(reason.contains("KnownTrusted"));
                assert!(reason.contains("other-conv"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn routine_list_returns_every_routine() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s4");
        let mk_ctx = || build_ctx_full(&db, cid.clone(), "Controller", false, false, false, true);
        CreateRoutineTool::new()
            .invoke(
                mk_ctx(),
                json!({"name": "a", "schedule_cron": "* * * * *", "prompt": "x"}),
            )
            .await;
        CreateRoutineTool::new()
            .invoke(
                mk_ctx(),
                json!({"name": "b", "schedule_cron": "* * * * *", "prompt": "y"}),
            )
            .await;
        let out = ListRoutinesTool::new().invoke(mk_ctx(), json!({})).await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["count"], 2);
                let routines = v["routines"].as_array().unwrap();
                let names: Vec<&str> = routines.iter().filter_map(|t| t["name"].as_str()).collect();
                assert!(names.contains(&"a"));
                assert!(names.contains(&"b"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Pause flips enabled to false; resume flips it back.
    #[tokio::test]
    async fn pause_then_resume_round_trip() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s5");
        let mk_ctx = || build_ctx_full(&db, cid.clone(), "Controller", false, false, false, true);

        let created = CreateRoutineTool::new()
            .invoke(
                mk_ctx(),
                json!({"name": "x", "schedule_cron": "* * * * *", "prompt": "p"}),
            )
            .await;
        let id = match created {
            ToolOutcome::Ok(v) => v["id"].as_str().unwrap().to_owned(),
            _ => panic!("create failed"),
        };

        let paused = PauseRoutineTool::new()
            .invoke(mk_ctx(), json!({"routine_id": &id}))
            .await;
        match paused {
            ToolOutcome::Ok(v) => assert_eq!(v["enabled"], false),
            _ => panic!("pause failed"),
        }
        let resumed = ResumeRoutineTool::new()
            .invoke(mk_ctx(), json!({"routine_id": &id}))
            .await;
        match resumed {
            ToolOutcome::Ok(v) => assert_eq!(v["enabled"], true),
            _ => panic!("resume failed"),
        }
    }

    #[tokio::test]
    async fn cancel_task_deletes_existing_and_returns_false_on_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s6");
        let mk_ctx = || build_ctx_full(&db, cid.clone(), "Controller", false, false, false, true);

        let created = CreateRoutineTool::new()
            .invoke(
                mk_ctx(),
                json!({"name": "x", "schedule_cron": "* * * * *", "prompt": "p"}),
            )
            .await;
        let id = match created {
            ToolOutcome::Ok(v) => v["id"].as_str().unwrap().to_owned(),
            _ => panic!("create failed"),
        };

        let del = DeleteRoutineTool::new()
            .invoke(mk_ctx(), json!({"routine_id": &id}))
            .await;
        match del {
            ToolOutcome::Ok(v) => assert_eq!(v["deleted"], true),
            _ => panic!("delete failed"),
        }
        // Second call: id no longer exists.
        let del2 = DeleteRoutineTool::new()
            .invoke(mk_ctx(), json!({"routine_id": &id}))
            .await;
        match del2 {
            ToolOutcome::Ok(v) => assert_eq!(v["deleted"], false),
            _ => panic!("delete-second failed"),
        }
    }

    #[tokio::test]
    async fn update_task_changes_only_supplied_fields() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s7");
        let mk_ctx = || build_ctx_full(&db, cid.clone(), "Controller", false, false, false, true);

        let created = CreateRoutineTool::new()
            .invoke(
                mk_ctx(),
                json!({"name": "old", "schedule_cron": "0 9 * * *", "prompt": "p"}),
            )
            .await;
        let id = match created {
            ToolOutcome::Ok(v) => v["id"].as_str().unwrap().to_owned(),
            _ => panic!("create failed"),
        };

        // Rename only.
        let updated = UpdateRoutineTool::new()
            .invoke(mk_ctx(), json!({"routine_id": &id, "name": "new"}))
            .await;
        match updated {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["name"], "new");
                // Cron stayed.
                assert_eq!(v["schedule_cron"], "0 9 * * *");
            }
            _ => panic!("update failed"),
        }
    }

    #[tokio::test]
    async fn schedule_tools_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "s8");
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match ListRoutinesTool::new().invoke(ctx, json!({})).await {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- web_fetch ----------------------------------------------------

    use crate::tool::{WebFetchApi, WebFetchResponse};

    /// Test stub for WebFetchApi — captures the URL the tool passed
    /// in and lets the test inject a canned response. Avoids hitting
    /// the network from `core`'s test suite.
    struct StubWebFetchApi {
        canned: WebFetchResponse,
    }

    #[async_trait]
    impl WebFetchApi for StubWebFetchApi {
        async fn get(&self, _url: &str) -> Result<WebFetchResponse, crate::tool::ApiError> {
            Ok(self.canned.clone())
        }
    }

    fn ctx_with_stub_web_fetch(
        db: &Database,
        cid: ConversationId,
        canned: WebFetchResponse,
    ) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid, "Controller", clock);
        ctx.web_fetch = Some(Arc::new(StubWebFetchApi { canned }));
        let _ = db; // db handle isn't needed for the stub variant.
        ctx
    }

    #[tokio::test]
    async fn web_fetch_returns_body_on_success() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "w1");
        let canned = WebFetchResponse {
            final_url: "https://example.com/article".into(),
            status: 200,
            content_type: Some("text/html".into()),
            body: "<html>hi</html>".into(),
            truncated: false,
        };
        let ctx = ctx_with_stub_web_fetch(&db, cid, canned);
        let out = WebFetchTool::new()
            .invoke(ctx, json!({"url": "https://example.com/article"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["status"], 200);
                assert_eq!(v["body"], "<html>hi</html>");
                assert_eq!(v["truncated"], false);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_truncates_long_body_to_default_3000_chars() {
        // Pre-fix the agent could consume entire pages (50-200KB ≈
        // 12-50K tokens), making 3-round web_search → web_fetch →
        // synthesise turns blow the model's context. The default
        // 3000-char cap keeps a typical fetched-page at ~750 tokens.
        let db = fresh_db();
        let cid = seed_conversation(&db, "w-trunc");
        let long_body: String = "a".repeat(20_000);
        let canned = WebFetchResponse {
            final_url: "https://example.com/long".into(),
            status: 200,
            content_type: Some("text/plain".into()),
            body: long_body,
            truncated: false,
        };
        let ctx = ctx_with_stub_web_fetch(&db, cid, canned);
        let out = WebFetchTool::new()
            .invoke(ctx, json!({"url": "https://example.com/long"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                let body = v["body"].as_str().unwrap();
                // 3000 chars + the "\n…" marker we append.
                assert!(
                    body.chars().count() <= 3002,
                    "body must be capped, got {} chars",
                    body.chars().count(),
                );
                assert!(body.ends_with('…'));
                assert_eq!(v["truncated"], true);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_honours_max_chars_override() {
        // Operator wants more — explicit max_chars wins (clamped at
        // 50000).
        let db = fresh_db();
        let cid = seed_conversation(&db, "w-bigger");
        let body: String = "x".repeat(10_000);
        let canned = WebFetchResponse {
            final_url: "https://example.com/x".into(),
            status: 200,
            content_type: Some("text/plain".into()),
            body,
            truncated: false,
        };
        let ctx = ctx_with_stub_web_fetch(&db, cid, canned);
        let out = WebFetchTool::new()
            .invoke(
                ctx,
                json!({"url": "https://example.com/x", "max_chars": 8000}),
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                let body = v["body"].as_str().unwrap();
                assert!(body.chars().count() <= 8002);
                assert!(body.chars().count() > 3000);
                assert_eq!(v["truncated"], true);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_does_not_split_a_multibyte_codepoint() {
        // Defensive: char-iterator slicing keeps UTF-8 boundaries
        // intact even when the cap lands mid-codepoint of a wide
        // glyph. A naive byte slice would corrupt the trailing
        // bytes and JSON serialisation downstream would fail.
        let db = fresh_db();
        let cid = seed_conversation(&db, "w-utf8");
        // Each emoji is 4 bytes / 1 char. 1000 of them = 4000
        // bytes, 1000 chars — well over the 256 minimum cap.
        let emoji_body: String = "🦀".repeat(1000);
        let canned = WebFetchResponse {
            final_url: "https://example.com/emoji".into(),
            status: 200,
            content_type: Some("text/plain".into()),
            body: emoji_body,
            truncated: false,
        };
        let ctx = ctx_with_stub_web_fetch(&db, cid, canned);
        let out = WebFetchTool::new()
            .invoke(
                ctx,
                json!({"url": "https://example.com/emoji", "max_chars": 500}),
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                let body = v["body"].as_str().unwrap();
                // Char count is 500 emoji + the "\n…" suffix (2
                // chars). Byte count would be 2000 + 4 if naive
                // slicing was used; we assert the cap as char
                // boundary not byte to make the intent explicit.
                assert_eq!(body.chars().filter(|c| *c == '🦀').count(), 500);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_invalid_args_returns_err() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "w2");
        let canned = WebFetchResponse {
            final_url: "x".into(),
            status: 0,
            content_type: None,
            body: "".into(),
            truncated: false,
        };
        let ctx = ctx_with_stub_web_fetch(&db, cid, canned);
        match WebFetchTool::new().invoke(ctx, json!({"u": "no"})).await {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "w3");
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match WebFetchTool::new()
            .invoke(ctx, json!({"url": "https://example.com"}))
            .await
        {
            ToolOutcome::Denied { reason } => {
                assert!(reason.contains("web_fetch"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- list_chats ---------------------------------------------------

    #[tokio::test]
    async fn list_chats_returns_every_visible_thread() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "lc1");
        seed_conversation(&db, "lc2");
        seed_conversation(&db, "lc3");
        let tool = ListChatsTool::new();
        let ctx = build_ctx(&db, cid, "Controller", true, false);
        let out = tool.invoke(ctx, json!({})).await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["count"], 3);
                let ids: Vec<&str> = v["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|t| t["conversation_id"].as_str())
                    .collect();
                assert!(ids.contains(&"lc1"));
                assert!(ids.contains(&"lc2"));
                assert!(ids.contains(&"lc3"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_chats_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "lc4");
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match ListChatsTool::new().invoke(ctx, json!({})).await {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- web_search ---------------------------------------------------

    use crate::tool::{SearchResult, WebSearchApi};

    struct StubSearchApi {
        canned: Vec<SearchResult>,
    }
    #[async_trait]
    impl WebSearchApi for StubSearchApi {
        fn provider_id(&self) -> &str {
            "stub"
        }
        async fn search(
            &self,
            _query: &str,
            _max: u32,
        ) -> Result<Vec<SearchResult>, crate::tool::ApiError> {
            Ok(self.canned.clone())
        }
    }

    fn ctx_with_stub_search(
        db: &Database,
        cid: ConversationId,
        canned: Vec<SearchResult>,
    ) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid, "Controller", clock);
        ctx.search = Some(Arc::new(StubSearchApi { canned }));
        let _ = db;
        ctx
    }

    #[tokio::test]
    async fn web_search_returns_results_with_provider_label() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ws1");
        let canned = vec![
            SearchResult {
                title: "First".into(),
                url: "https://example.com/1".into(),
                snippet: Some("snippet 1".into()),
            },
            SearchResult {
                title: "Second".into(),
                url: "https://example.org/2".into(),
                snippet: None,
            },
        ];
        let ctx = ctx_with_stub_search(&db, cid, canned);
        let out = WebSearchTool::new()
            .invoke(ctx, json!({"query": "foo"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["provider"], "stub");
                assert_eq!(v["count"], 2);
                assert_eq!(v["results"][0]["title"], "First");
                assert_eq!(v["results"][1]["snippet"], Value::Null);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_search_rejects_empty_query() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ws2");
        let ctx = ctx_with_stub_search(&db, cid, vec![]);
        match WebSearchTool::new()
            .invoke(ctx, json!({"query": "  "}))
            .await
        {
            ToolOutcome::Err { code, message } => {
                assert_eq!(code, "invalid_argument");
                assert!(message.contains("empty"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_search_clamps_max_results_to_25() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ws3");
        // Stub returns whatever the tool asked for so we can verify
        // clamping by inspecting what reaches the trait.
        struct ClampSpy {
            captured: std::sync::Mutex<u32>,
        }
        #[async_trait]
        impl WebSearchApi for ClampSpy {
            fn provider_id(&self) -> &str {
                "clamp"
            }
            async fn search(
                &self,
                _q: &str,
                max: u32,
            ) -> Result<Vec<SearchResult>, crate::tool::ApiError> {
                *self.captured.lock().unwrap() = max;
                Ok(vec![])
            }
        }
        let spy = Arc::new(ClampSpy {
            captured: std::sync::Mutex::new(0),
        });
        let mut ctx = ToolCtx::empty(cid, "Controller", Arc::new(SystemClock));
        ctx.search = Some(spy.clone() as Arc<dyn WebSearchApi>);
        WebSearchTool::new()
            .invoke(ctx, json!({"query": "x", "max_results": 100}))
            .await;
        assert_eq!(*spy.captured.lock().unwrap(), 25);
    }

    #[tokio::test]
    async fn web_search_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "ws4");
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match WebSearchTool::new()
            .invoke(ctx, json!({"query": "x"}))
            .await
        {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    // --- delegate_task ------------------------------------------------

    use crate::tool::{SubagentApi, SubagentRequest, SubagentResponse};

    struct StubSubagentApi {
        canned: SubagentResponse,
    }

    #[async_trait]
    impl SubagentApi for StubSubagentApi {
        async fn delegate(
            &self,
            _req: &SubagentRequest,
        ) -> Result<SubagentResponse, crate::tool::ApiError> {
            Ok(self.canned.clone())
        }
    }

    fn ctx_with_stub_subagent(
        db: &Database,
        cid: ConversationId,
        canned: SubagentResponse,
    ) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid, "Controller", clock);
        ctx.subagent = Some(Arc::new(StubSubagentApi { canned }));
        let _ = db;
        ctx
    }

    #[tokio::test]
    async fn delegate_task_returns_text_with_task_id() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "d1");
        let canned = SubagentResponse {
            text: "draft body here".into(),
            task_id: "abc-123".into(),
            tokens_used: Some(42),
        };
        let ctx = ctx_with_stub_subagent(&db, cid, canned);
        let out = DelegateTaskTool::new()
            .invoke(ctx, json!({"task": "draft an email"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["text"], "draft body here");
                assert_eq!(v["task_id"], "abc-123");
                assert_eq!(v["tokens_used"], 42);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_task_rejects_empty_task() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "d2");
        let canned = SubagentResponse {
            text: "".into(),
            task_id: "x".into(),
            tokens_used: None,
        };
        let ctx = ctx_with_stub_subagent(&db, cid, canned);
        match DelegateTaskTool::new()
            .invoke(ctx, json!({"task": "  "}))
            .await
        {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_task_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "d3");
        let ctx = build_ctx(&db, cid, "Controller", false, false);
        match DelegateTaskTool::new()
            .invoke(ctx, json!({"task": "x"}))
            .await
        {
            ToolOutcome::Denied { reason } => {
                assert!(reason.contains("subagent"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_task_caps_max_tokens_to_4096() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "d4");
        // Spy that captures the request's max_tokens.
        struct CaptureSpy {
            captured: std::sync::Mutex<Option<u32>>,
        }
        #[async_trait]
        impl SubagentApi for CaptureSpy {
            async fn delegate(
                &self,
                req: &SubagentRequest,
            ) -> Result<SubagentResponse, crate::tool::ApiError> {
                *self.captured.lock().unwrap() = req.max_tokens;
                Ok(SubagentResponse {
                    text: "ok".into(),
                    task_id: "id".into(),
                    tokens_used: None,
                })
            }
        }
        let spy = Arc::new(CaptureSpy {
            captured: std::sync::Mutex::new(None),
        });
        let mut ctx = ToolCtx::empty(cid, "Controller", Arc::new(SystemClock));
        ctx.subagent = Some(spy.clone() as Arc<dyn SubagentApi>);
        DelegateTaskTool::new()
            .invoke(ctx, json!({"task": "x", "max_tokens": 10_000}))
            .await;
        assert_eq!(*spy.captured.lock().unwrap(), Some(4096));
    }

    // --- research_* ---------------------------------------------------

    use crate::tool_apis::DbResearchApi;

    fn build_ctx_with_research_spawn(db: &Database, cid: ConversationId, trust: &str) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid.clone(), trust, clock.clone());
        ctx.research = Some(Arc::new(DbResearchApi::with_spawn(
            db.clone(),
            trust,
            cid,
            clock.now_unix(),
        )));
        ctx
    }

    fn build_ctx_with_research_read(db: &Database, cid: ConversationId, trust: &str) -> ToolCtx {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut ctx = ToolCtx::empty(cid.clone(), trust, clock.clone());
        ctx.research = Some(Arc::new(DbResearchApi::read_only(
            db.clone(),
            trust,
            cid,
            clock.now_unix(),
        )));
        ctx
    }

    #[tokio::test]
    async fn research_start_inserts_pending_row_and_returns_job_id() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let ctx = build_ctx_with_research_spawn(&db, cid.clone(), "Controller");
        let out = ResearchStartTool::new()
            .invoke(ctx, json!({"query": "what's new in Kokoro?"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                let job_id = v["job"]["id"].as_str().unwrap();
                assert!(!job_id.is_empty());
                assert_eq!(v["job"]["status"], "pending");
                assert_eq!(v["job"]["query"], "what's new in Kokoro?");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_start_denied_when_capability_missing() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        // Empty ToolCtx — no `research` populated.
        let ctx = ToolCtx::empty(cid, "Controller", Arc::new(SystemClock));
        let out = ResearchStartTool::new()
            .invoke(ctx, json!({"query": "hi"}))
            .await;
        match out {
            ToolOutcome::Denied { reason } => {
                assert!(reason.contains("research_spawn"), "got: {reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_start_denied_when_only_read_capability_granted() {
        // Adversarial test: a tool dispatcher that wired a read-only
        // ResearchApi (because the descriptor only declared
        // ResearchRead) must NOT let the caller spawn a job. The
        // DbResearchApi's `can_spawn = false` flag is what enforces
        // this — the tool sees `Some(api)` but `start` returns
        // NotAuthorized.
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let ctx = build_ctx_with_research_read(&db, cid, "Controller");
        let out = ResearchStartTool::new()
            .invoke(ctx, json!({"query": "hi"}))
            .await;
        match out {
            ToolOutcome::Denied { reason } => {
                assert!(reason.contains("research_spawn"), "got: {reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_start_rejects_empty_query() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let ctx = build_ctx_with_research_spawn(&db, cid, "Controller");
        let out = ResearchStartTool::new()
            .invoke(ctx, json!({"query": "   "}))
            .await;
        match out {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_status_returns_inserted_row() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        // Spawn a job first via the spawn-enabled ctx, then read it
        // via a read-only ctx — proves the read-only path can see
        // jobs the spawn path created.
        let spawn_ctx = build_ctx_with_research_spawn(&db, cid.clone(), "Controller");
        let started = ResearchStartTool::new()
            .invoke(spawn_ctx, json!({"query": "anything"}))
            .await;
        let job_id = match started {
            ToolOutcome::Ok(v) => v["job"]["id"].as_str().unwrap().to_owned(),
            other => panic!("seed: {other:?}"),
        };
        let read_ctx = build_ctx_with_research_read(&db, cid, "Controller");
        let out = ResearchStatusTool::new()
            .invoke(read_ctx, json!({"job_id": job_id}))
            .await;
        match out {
            ToolOutcome::Ok(v) => assert_eq!(v["job"]["status"], "pending"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_status_returns_not_found_for_other_conversation_when_low_trust() {
        // Trust-scope adversarial test: a KnownTrusted caller in
        // conversation A asks for a job id that lives in conversation
        // B. The DbResearchApi must answer NotFound (not NotAuthorized
        // and not Ok) so the caller learns nothing about whether the
        // id exists.
        let db = fresh_db();
        let _conv_a = seed_conversation(&db, "conv-a");
        let _conv_b = seed_conversation(&db, "conv-b");
        // Seed a job in conversation B.
        let spawn_ctx =
            build_ctx_with_research_spawn(&db, ConversationId::from("conv-b"), "Controller");
        let started = ResearchStartTool::new()
            .invoke(spawn_ctx, json!({"query": "B's job"}))
            .await;
        let job_id = match started {
            ToolOutcome::Ok(v) => v["job"]["id"].as_str().unwrap().to_owned(),
            other => panic!("seed: {other:?}"),
        };
        // Now KnownTrusted caller in conversation A asks about it.
        let read_ctx =
            build_ctx_with_research_read(&db, ConversationId::from("conv-a"), "KnownTrusted");
        let out = ResearchStatusTool::new()
            .invoke(read_ctx, json!({"job_id": job_id}))
            .await;
        match out {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "not_found"),
            other => panic!("expected Err(not_found), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_list_controller_sees_every_conversation() {
        let db = fresh_db();
        let _ = seed_conversation(&db, "conv-a");
        let _ = seed_conversation(&db, "conv-b");
        for conv in ["conv-a", "conv-b"] {
            let ctx = build_ctx_with_research_spawn(&db, ConversationId::from(conv), "Controller");
            let _ = ResearchStartTool::new()
                .invoke(ctx, json!({"query": format!("q in {conv}")}))
                .await;
        }
        // Controller scopes globally even when the caller is in conv-a.
        let read_ctx =
            build_ctx_with_research_read(&db, ConversationId::from("conv-a"), "Controller");
        let out = ResearchListTool::new().invoke(read_ctx, json!({})).await;
        match out {
            ToolOutcome::Ok(v) => assert_eq!(v["count"], 2),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_get_report_returns_null_when_no_workspace_yet() {
        // A freshly-spawned job has no workspace_path on the row
        // until the runner provisions it. The tool must return
        // a `null` report rather than erroring so the caller can
        // poll cleanly.
        let db = fresh_db();
        let cid = seed_conversation(&db, "conv-report-null");
        // Spawn a job (status: Pending, workspace_path: NULL).
        let spawn_ctx = build_ctx_with_research_spawn(&db, cid.clone(), "Controller");
        let started = ResearchStartTool::new()
            .invoke(spawn_ctx, json!({"query": "anything"}))
            .await;
        let job_id = match started {
            ToolOutcome::Ok(v) => v["job"]["id"].as_str().unwrap().to_owned(),
            other => panic!("seed: {other:?}"),
        };
        let read_ctx = build_ctx_with_research_read(&db, cid, "Controller");
        match ResearchGetReportTool::new()
            .invoke(read_ctx, json!({"job_id": job_id}))
            .await
        {
            ToolOutcome::Ok(v) => {
                assert!(v["report_markdown"].is_null());
            }
            other => panic!("expected Ok with null report, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_get_report_reads_workspace_when_report_exists() {
        // Simulate a completed job: insert pending → set workspace
        // path to a temp dir → drop a report.md → ask the tool.
        use crate::research::ResearchJobStore;
        let db = fresh_db();
        let cid = seed_conversation(&db, "conv-report-have");
        let store = ResearchJobStore::new(&db);
        let id = crate::ids::ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join(id.as_str());
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(
            workspace_dir.join("report.md"),
            "# Final report\n\nFindings.\n",
        )
        .unwrap();
        store
            .set_workspace_path(&id, &workspace_dir.to_string_lossy(), 200)
            .unwrap();
        let read_ctx = build_ctx_with_research_read(&db, cid, "Controller");
        match ResearchGetReportTool::new()
            .invoke(read_ctx, json!({"job_id": id.as_str()}))
            .await
        {
            ToolOutcome::Ok(v) => {
                let body = v["report_markdown"].as_str().unwrap();
                assert!(body.contains("Final report"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_get_report_denies_low_trust_caller_in_other_conversation() {
        // Adversarial — a KnownTrusted caller in conv-A asks for
        // the report belonging to conv-B. Must NOT leak the report.
        use crate::research::ResearchJobStore;
        let db = fresh_db();
        let _ = seed_conversation(&db, "conv-A");
        let _ = seed_conversation(&db, "conv-B");
        let cid_b = ConversationId::from("conv-B");
        let store = ResearchJobStore::new(&db);
        let id = crate::ids::ResearchJobId::new();
        store
            .insert_pending(&id, &cid_b, "q", "Controller", None, 100)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join(id.as_str());
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(
            workspace_dir.join("report.md"),
            "secret report belonging to conv-B",
        )
        .unwrap();
        store
            .set_workspace_path(&id, &workspace_dir.to_string_lossy(), 200)
            .unwrap();
        let read_ctx =
            build_ctx_with_research_read(&db, ConversationId::from("conv-A"), "KnownTrusted");
        match ResearchGetReportTool::new()
            .invoke(read_ctx, json!({"job_id": id.as_str()}))
            .await
        {
            ToolOutcome::Ok(v) => {
                // Must surface as null (job hidden from caller's
                // view), not the real report.
                assert!(
                    v["report_markdown"].is_null(),
                    "leaked cross-conversation report: {v}",
                );
            }
            other => panic!("expected Ok with null report, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_list_low_trust_only_sees_own_conversation() {
        let db = fresh_db();
        let _ = seed_conversation(&db, "conv-a");
        let _ = seed_conversation(&db, "conv-b");
        for conv in ["conv-a", "conv-b"] {
            let ctx = build_ctx_with_research_spawn(&db, ConversationId::from(conv), "Controller");
            let _ = ResearchStartTool::new()
                .invoke(ctx, json!({"query": format!("q in {conv}")}))
                .await;
        }
        // KnownTrusted caller in conv-a sees only conv-a's job.
        let read_ctx =
            build_ctx_with_research_read(&db, ConversationId::from("conv-a"), "KnownTrusted");
        let out = ResearchListTool::new().invoke(read_ctx, json!({})).await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["count"], 1);
                assert_eq!(v["jobs"][0]["conversation_id"], "conv-a");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }
}
