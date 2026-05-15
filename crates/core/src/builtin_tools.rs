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
//! - `list_memory` — prefix scan over the caller's read-down chain,
//!   excluding COLD entries (post migration 0035).
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
                description: "Permanently delete a routine. Returns `{deleted: true}` on success, \
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
    // When `status == "awaiting_input"`, the planner stored its
    // clarification question in the `error` column (intentional reuse
    // — see `ResearchJobStore::set_awaiting_input`'s docs). Surface
    // it under a dedicated key so the agent's tool-result reader
    // doesn't have to know about that storage detail. The `error`
    // field remains populated for backward-compat readers (the SPA
    // card derives the question from this same value).
    let clarification_question = if v.status == "awaiting_input" {
        v.error.clone()
    } else {
        None
    };
    json!({
        "id": v.id,
        "conversation_id": v.conversation_id,
        "query": v.query,
        "status": v.status,
        "card_id": v.card_id,
        "workspace_path": v.workspace_path,
        "attachment_id": v.attachment_id,
        "error": v.error,
        "clarification_question": clarification_question,
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
                    "Start a deep-research job for a question. Returns immediately with a \
                     Pending job_id; the runner picks it up asynchronously and the planner / \
                     gather / synthesise phases run in the background. For sub-minute focused \
                     work use `delegate_task` instead.\n\n\
                     WHAT TO TELL THE USER: briefly acknowledge that you've started the \
                     research and that you'll deliver the report when it's ready. Don't \
                     describe the UI surface (no \"plan card,\" \"chip,\" \"download \
                     button\" — those phrases assume a web client; the user might be on \
                     Signal, email, or another transport). End your turn — do NOT call \
                     `research_status` in a loop.\n\n\
                     CLARIFICATION FLOW (event-driven, no action needed from you on the \
                     start turn): if the planner judges the query too vague to plan, the \
                     server will wake you in a follow-up turn with a system-orchestrator \
                     prompt carrying the planner's question. At that point, relay the \
                     question to the user; on their next reply, call \
                     `research_clarify(job_id, answer)` to resume the job. The original \
                     research_start job stays alive across the pause — never call \
                     research_start a second time for the same query.\n\n\
                     COMPLETION: when the synthesise phase finishes, the runner auto- \
                     delivers the PDF report through whichever channel(s) the conversation \
                     is reachable on (web download chip + Signal attachment + future \
                     transports). You don't need to call `send_attachment` for the \
                     completion event. Only call `send_attachment(attachment_id)` if a \
                     downstream user explicitly asks you to re-surface the file."
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
                     final report (if complete), and any error.\n\n\
                     DELIVERY: the host auto-dispatches the PDF via every channel the \
                     conversation is reachable on the moment status flips to `complete` — \
                     you do NOT need to call `send_attachment` to surface the completion \
                     deliverable. Only call `send_attachment(attachment_id)` if the user \
                     explicitly asks you to resend the file later. Never paste the raw \
                     report text into your reply — the PDF is the deliverable."
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

// ---------------------------------------------------------------
// send_attachment — agent delivers a file inline to the conversation.
// Symmetric to channel plugins' send_attachment for TextOnly transports;
// this is the web-channel equivalent (emits an Attachment card the SPA
// renders as an inline download chip).
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SendAttachmentArgs {
    /// AttachmentId (the row id from `state_attachments`). For
    /// research deliverables, this is the value returned in the
    /// research-job's `attachment_id` field after the synthesise
    /// phase landed the report.
    attachment_id: String,
    /// Optional one-liner shown alongside the file chip in chat
    /// (e.g. "Final research report on evergreen ground covers").
    /// When omitted the chip falls back to the file basename.
    #[serde(default)]
    caption: Option<String>,
}

fn default_allowed_for_attachment_send() -> Vec<String> {
    // Same trust ladder as research_spawn — the agent can deliver
    // attachments only when it has spawn-class trust. A future
    // tightening could split this further, but for now: agents that
    // can spawn research can also deliver the resulting PDF.
    vec!["Controller".into(), "Owner".into(), "KnownTrusted".into()]
}

pub struct SendAttachmentTool {
    descriptor: ToolDescriptor,
}

impl Default for SendAttachmentTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SendAttachmentTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "send_attachment".into(),
                description:
                    "Deliver a file to the user. Use this whenever the user asks you to send / share / \
                     resend a file (e.g. a deep-research PDF). The host fans the file out across every \
                     channel the conversation is reachable on — the user gets it in the web UI as a \
                     download chip AND on whichever transport the conversation is bridged through \
                     (Signal, etc.) without you having to pick a channel-specific tool. \
                     Pass the `attachment_id` returned by the producing tool — for research, that's \
                     the `attachment_id` field on a completed `research_status` result. Optional \
                     `caption` shows above the chip and as the message body on transports \
                     (defaults to the file basename). \
                     Prefer this over channel-specific send_message tools when your goal is just \
                     to deliver a file — those are for sending text + attachment together to a \
                     specific recipient on a specific channel."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "attachment_id": {
                            "type": "string",
                            "description": "Id of the attachment row to deliver (e.g. from research_status)."
                        },
                        "caption": {
                            "type": "string",
                            "description": "Optional one-line caption shown above the chip."
                        }
                    },
                    "required": ["attachment_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::AttachmentSend],
                default_allowed_classes: default_allowed_for_attachment_send(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for SendAttachmentTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: SendAttachmentArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.attachments.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied("attachment_send capability not granted to this tool");
            }
        };
        match api.send(&args.attachment_id, args.caption.as_deref()).await {
            Ok(view) => ToolOutcome::Ok(json!({
                "attachment_id": view.attachment_id,
                "filename": view.filename,
                "mime_type": view.mime_type,
                "byte_size": view.byte_size,
                "download_url": view.download_url,
                "caption": view.caption,
                "delivered": true,
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// chart.render — pure-Rust Vega-Lite-ish chart renderer.
//
// 2026-05-15 — promoted from the open-meteo plugin's
// `open_meteo.render_chart` to a native built-in. The
// implementation was already 100% native (the plugin just called
// `host_render_chart` which lived in the script-tier); the only
// reason it lived as a plugin tool was that built-ins had no way
// to produce attachments. With `AttachmentApi::create_artifact`
// (added the same day) that gap closes and the tool moves where
// it belongs — every plugin that wants to chart its data can now
// route through one tool name (`chart.render`) instead of
// re-exposing it under a per-plugin namespace.
//
// The renderer accepts a structured spec (line / bar / area /
// scatter, optional band overlay, optional time axis) and
// produces both an inline SVG (for the SPA's chat-component
// dispatcher) and a PNG attachment (for transport fan-out via
// `send_attachment` or a channel's `send_with_attachments`). The
// SVG is returned in the tool result; the PNG is persisted as a
// state_artifacts row whose `attachment_id` flows into downstream
// tool calls.
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RenderChartArgs {
    /// Operator-friendly spec — flattened from the open-meteo
    /// `render_chart_impl` shape so plugin authors can copy-paste
    /// their existing builders. Forwarded verbatim to the
    /// charting crate's `serde_json::from_value::<ChartSpec>`.
    #[serde(flatten)]
    spec: serde_json::Value,
    /// Canvas width in px. Clamped to [240, 2400]; 0 / missing
    /// uses the renderer's default (720).
    #[serde(default)]
    width: Option<u32>,
    /// Canvas height in px. Clamped to [240, 2400]; 0 / missing
    /// uses the renderer's default (400).
    #[serde(default)]
    height: Option<u32>,
    /// Filename for the PNG attachment (operator's save-as
    /// dialog). Default `"chart.png"`.
    #[serde(default)]
    filename: Option<String>,
    /// How long the PNG artifact lives before the ephemeral
    /// sweeper removes it. `None` / 0 = keep forever; positive =
    /// seconds. Default 7 days.
    #[serde(default)]
    ttl_seconds: Option<i64>,
}

const RENDER_CHART_MIN_DIM: u32 = 240;
const RENDER_CHART_MAX_DIM: u32 = 2400;
const RENDER_CHART_DEFAULT_TTL_SECS: i64 = 7 * 86400;

fn clamp_render_chart_dim(value: Option<u32>, default: u32) -> u32 {
    let v = value.unwrap_or(0);
    if v == 0 {
        default
    } else {
        v.clamp(RENDER_CHART_MIN_DIM, RENDER_CHART_MAX_DIM)
    }
}

pub struct RenderChartTool {
    descriptor: ToolDescriptor,
}

impl Default for RenderChartTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderChartTool {
    pub fn new() -> Self {
        // Schema mirrors the original `plugins/open-meteo/schemas/render_chart.json`.
        // Kept here in code rather than as a sidecar JSON file so the
        // built-in catalog stays self-contained (the same pattern
        // the other built-ins follow).
        let schema = json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Chart title rendered at the top of the SVG/PNG."
                },
                "kind": {
                    "type": "string",
                    "enum": ["line", "bar", "area", "scatter"],
                    "description": "Default \"line\"."
                },
                "x_label": { "type": "string" },
                "y_label": { "type": "string" },
                "y_unit": {
                    "type": "string",
                    "description": "Suffix appended to y-axis tick labels (e.g. \"°C\", \" mm\")."
                },
                "time_axis": {
                    "type": "boolean",
                    "description": "When true, x-values are interpreted as Unix-milliseconds and the axis renders as HH:MM / MMM-DD."
                },
                "series": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "points": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["x", "y"]
                                }
                            },
                            "color": {
                                "type": "array",
                                "items": { "type": "integer", "minimum": 0, "maximum": 255 },
                                "minItems": 3,
                                "maxItems": 3
                            }
                        },
                        "required": ["name", "points"]
                    }
                },
                "band": {
                    "type": "object",
                    "description": "Optional probability / range overlay (ensemble fans).",
                    "properties": {
                        "low":  { "type": "array" },
                        "high": { "type": "array" },
                        "color": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0, "maximum": 255 },
                            "minItems": 3,
                            "maxItems": 3
                        }
                    },
                    "required": ["low", "high"]
                },
                "width":  { "type": "integer", "minimum": 240, "maximum": 2400 },
                "height": { "type": "integer", "minimum": 240, "maximum": 2400 },
                "filename": { "type": "string" },
                "ttl_seconds": { "type": "integer", "minimum": 0 }
            }
        });
        Self {
            descriptor: ToolDescriptor {
                name: "chart.render".into(),
                description: concat!(
                    "Render a chart from a structured spec (line / bar / area / scatter, ",
                    "with optional band overlay for ensemble fans). Returns BOTH an inline ",
                    "SVG string (for the SPA chat card to render directly) AND a PNG ",
                    "attachment_id (for delivery via `send_attachment` or a channel's ",
                    "`send_with_attachments`). Use this whenever the user asks for a chart, ",
                    "graph, or visualisation — pass the data as `series: [{name, points: ",
                    "[{x, y}]}]`. Set `time_axis: true` when x-values are Unix-milliseconds. ",
                    "Width/height in px (clamped 240-2400, default 720x400). The PNG ",
                    "attachment expires after `ttl_seconds` (default 7 days)."
                ).into(),
                schema,
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::AttachmentSend],
                default_allowed_classes: default_allowed_for_attachment_send(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for RenderChartTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let parsed: RenderChartArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.attachments.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied(
                    "attachment_send capability not granted to this tool",
                );
            }
        };
        // Decode the spec via the charting crate's own ChartSpec
        // shape — flattening the operator-supplied JSON into the
        // renderer's struct so unsupported fields surface a clear
        // error instead of being silently dropped.
        let spec: execlaw_charting::ChartSpec =
            match serde_json::from_value(parsed.spec.clone()) {
                Ok(s) => s,
                Err(e) => {
                    return ToolOutcome::err(
                        "invalid_spec",
                        format!("chart.render: invalid spec: {e}"),
                    );
                }
            };
        let width = clamp_render_chart_dim(parsed.width, execlaw_charting::DEFAULT_WIDTH);
        let height = clamp_render_chart_dim(parsed.height, execlaw_charting::DEFAULT_HEIGHT);
        // Plotters renders are 1-20ms in practice; keep it inline.
        let svg = match execlaw_charting::render_to_svg(&spec, width, height) {
            Ok(s) => s,
            Err(e) => {
                return ToolOutcome::err(
                    "render_failed",
                    format!("chart.render: svg: {e}"),
                );
            }
        };
        let png = match execlaw_charting::render_to_png(&spec, width, height) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutcome::err(
                    "render_failed",
                    format!("chart.render: png: {e}"),
                );
            }
        };
        let filename = parsed
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("chart.png")
            .to_owned();
        // TTL: None / 0 = renderer's default (7d). Positive = explicit seconds.
        let ttl = match parsed.ttl_seconds {
            None => Some(RENDER_CHART_DEFAULT_TTL_SECS),
            Some(0) => None,
            Some(n) if n > 0 => Some(n),
            Some(neg) => {
                return ToolOutcome::err(
                    "invalid_argument",
                    format!("chart.render: ttl_seconds must be >= 0 (got {neg})"),
                );
            }
        };
        match api.create_artifact(&filename, "image/png", png, ttl).await {
            Ok(view) => ToolOutcome::Ok(json!({
                "attachment_id": view.attachment_id,
                "sha256": view.sha256,
                "size_bytes": view.size_bytes,
                "filename": filename,
                "mime_type": "image/png",
                "width": width,
                "height": height,
                // The SPA's chat-component dispatcher reads `svg` to
                // render inline without a follow-up fetch.
                "svg": svg,
                // Hint the SPA on which chat-component to mount —
                // the existing convention for plugin tool results.
                "chat_component_kind": "chart",
            })),
            Err(e) => e.into_outcome(),
        }
    }
}

// ---------------------------------------------------------------
// research_clarify — resume an awaiting_input job with the user's
// answer. Per project-locked-decisions 2026-04-23, the agent is the
// primary interface for the clarification path; this tool is the
// glue between "user answered the planner's question in chat" and
// "runner gets the augmented query and re-plans."
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ResearchClarifyArgs {
    job_id: String,
    /// The operator's answer to the planner's clarification question,
    /// captured by the agent in chat. Will be appended to the
    /// original query so the next planner pass sees both pieces of
    /// context.
    clarification: String,
}

pub struct ResearchClarifyTool {
    descriptor: ToolDescriptor,
}

impl Default for ResearchClarifyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchClarifyTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "research_clarify".into(),
                description:
                    "Resume a deep-research job that is paused in `awaiting_input` by feeding \
                     back the operator's answer to the planner's clarification question. The \
                     answer is appended to the original query and the job re-enters the planner \
                     queue automatically. Call this only after a job's `research_status` shows \
                     status=`awaiting_input` and the user has answered the question in chat. \
                     Returns the updated job view (status will be `pending` again)."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "The id returned by `research_start`."
                        },
                        "clarification": {
                            "type": "string",
                            "description": "The operator's answer, captured in chat. Will be \
                                           appended to the original query for the planner to \
                                           re-plan with."
                        }
                    },
                    "required": ["job_id", "clarification"],
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
impl ToolImpl for ResearchClarifyTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ResearchClarifyArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.research.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied(
                    "research_spawn capability not granted to this tool (clarify is a write)",
                );
            }
        };
        match api.clarify(&args.job_id, &args.clarification).await {
            Ok(view) => ToolOutcome::Ok(json!({"job": job_view_to_json(&view)})),
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
// MCP server lifecycle (Controller-trust only)
// ---------------------------------------------------------------

fn default_allowed_for_mcp_admin() -> Vec<String> {
    // MCP admin is sensitive — adding a server pulls in a new tool
    // surface that the agent then has unrestricted access to. Pin
    // to Controller-class only. Operators can NOT broaden this in
    // Settings → Tools without an explicit security review.
    vec!["Controller".into()]
}

#[derive(Debug, Deserialize)]
struct McpAddServerArgs {
    id: String,
    display_name: String,
    /// Currently only "streamable_http" is accepted from the agent.
    /// Stdio specs are rejected here (arbitrary-binary risk);
    /// operators add stdio servers via the SPA.
    transport: String,
    url: Option<String>,
    /// Bearer token sent as `Authorization: Bearer <token>`.
    /// Stored encrypted in the vault under a generated key.
    auth_token: Option<String>,
}

pub struct McpListServersTool {
    descriptor: ToolDescriptor,
}

impl Default for McpListServersTool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpListServersTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "mcp_list_servers".into(),
                description: "List every configured MCP server. Use BEFORE proposing a new \
                     `mcp_add_server` call so you don't double-add a server the \
                     operator already wired up. Returns a list of {id, display_name, \
                     transport, url, command, enabled, status, last_error, tool_count} \
                     per server."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::McpAdmin],
                default_allowed_classes: default_allowed_for_mcp_admin(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for McpListServersTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let api = match ctx.mcp_admin.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied(
                    "mcp_admin capability not granted to this caller (Controller-trust required)",
                );
            }
        };
        match api.list_servers().await {
            Ok(servers) => match serde_json::to_value(json!({ "servers": servers })) {
                Ok(v) => ToolOutcome::Ok(v),
                Err(e) => ToolOutcome::err("encode", e.to_string()),
            },
            Err(e) => e.into_outcome(),
        }
    }
}

pub struct McpAddServerTool {
    descriptor: ToolDescriptor,
}

impl Default for McpAddServerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpAddServerTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "mcp_add_server".into(),
                description: "Add and immediately connect a new MCP server. Use this when the \
                     user asks the agent to integrate / install / wire up an MCP \
                     server (e.g. \"agent, integrate the Atlassian MCP server\").\n\n\
                     CONNECT MODEL: only `streamable_http` is supported here — agent \
                     cannot install stdio servers (those run arbitrary local \
                     binaries; operators add them manually via Settings → MCP).\n\n\
                     CLARIFICATION FLOW: most servers need a bearer token / API key. \
                     If you don't have one, do NOT make one up — ASK the user for \
                     it in plain text in your reply. Same pattern as the research \
                     clarification flow: stop and ask, the user replies with the \
                     token, you call `mcp_add_server` with `auth_token` populated. \
                     For Atlassian Rovo specifically: use \
                     `https://mcp.atlassian.com/v1/mcp/authv2` and ask the user to \
                     generate an API token from id.atlassian.com → Security → API \
                     tokens.\n\n\
                     ID is a slug 2-32 chars, alphanumeric + `-` + `_`, no leading/ \
                     trailing hyphen. After the call returns, the server's tools \
                     auto-flow into your tool catalog as `mcp:<id>:<name>` — you \
                     can call them on the next turn without further setup."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Slug used as the `mcp:<id>:<tool>` prefix. 2-32 chars, alphanumeric + '-' + '_', no leading/trailing '-'."
                        },
                        "display_name": {
                            "type": "string",
                            "description": "Human-readable label shown in Settings → MCP."
                        },
                        "transport": {
                            "type": "string",
                            "enum": ["streamable_http"],
                            "description": "Only streamable_http is accepted from the agent. stdio is operator-only."
                        },
                        "url": {
                            "type": "string",
                            "description": "Required for streamable_http. Full URL the MCP client POSTs JSON-RPC envelopes to."
                        },
                        "auth_token": {
                            "type": ["string", "null"],
                            "description": "Bearer token for the Authorization header. Omit if the server is unauthenticated. Ask the user for this if the server's docs require authentication."
                        }
                    },
                    "required": ["id", "display_name", "transport"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Medium,
                capabilities: vec![Capability::McpAdmin],
                default_allowed_classes: default_allowed_for_mcp_admin(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for McpAddServerTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let parsed: McpAddServerArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.mcp_admin.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied(
                    "mcp_admin capability not granted to this caller (Controller-trust required)",
                );
            }
        };
        let spec = crate::tool::McpServerSpec {
            id: parsed.id,
            display_name: parsed.display_name,
            transport: parsed.transport,
            url: parsed.url,
            auth_token: parsed.auth_token,
            command: None,
            args: vec![],
            env: std::collections::HashMap::new(),
        };
        match api.add_server(spec).await {
            Ok(view) => match serde_json::to_value(view) {
                Ok(v) => ToolOutcome::Ok(v),
                Err(e) => ToolOutcome::err("encode", e.to_string()),
            },
            Err(e) => e.into_outcome(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct McpRemoveServerArgs {
    id: String,
}

pub struct McpRemoveServerTool {
    descriptor: ToolDescriptor,
}

impl Default for McpRemoveServerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRemoveServerTool {
    pub fn new() -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "mcp_remove_server".into(),
                description: "Remove a configured MCP server. Stops the connection actor, \
                     drops the row from `state_mcp_servers`, marks every \
                     `mcp:<id>:*` tool removed in your catalog. Idempotent — \
                     removing a non-existent id returns NotFound. Ask the user \
                     for confirmation before calling this; uninstalls are not \
                     reversible (they have to re-add the server)."
                    .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "MCP server id (the slug, not the prefixed tool name)."
                        }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![Capability::McpAdmin],
                default_allowed_classes: default_allowed_for_mcp_admin(),
            },
        }
    }
}

#[async_trait]
impl ToolImpl for McpRemoveServerTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let parsed: McpRemoveServerArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let api = match ctx.mcp_admin.as_ref() {
            Some(a) => a,
            None => {
                return ToolOutcome::denied(
                    "mcp_admin capability not granted to this caller (Controller-trust required)",
                );
            }
        };
        match api.remove_server(&parsed.id).await {
            Ok(()) => ToolOutcome::Ok(json!({ "ok": true, "id": parsed.id })),
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
        Arc::new(ResearchClarifyTool::new()),
        Arc::new(ResearchListTool::new()),
        Arc::new(ResearchGetReportTool::new()),
        Arc::new(SendAttachmentTool::new()),
        Arc::new(RenderChartTool::new()),
        Arc::new(McpListServersTool::new()),
        Arc::new(McpAddServerTool::new()),
        Arc::new(McpRemoveServerTool::new()),
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
                display_name_source: "auto".into(),
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
        assert!(names.contains(&"research_clarify"));
        assert!(names.contains(&"research_list"));
        assert!(names.contains(&"research_get_report"));
        assert!(names.contains(&"send_attachment"));
        assert!(
            names.contains(&"chart.render"),
            "chart.render built-in must be registered (replaces the old plugin tool open_meteo.render_chart)",
        );
        assert!(names.contains(&"mcp_list_servers"));
        assert!(names.contains(&"mcp_add_server"));
        assert!(names.contains(&"mcp_remove_server"));
        assert_eq!(names.len(), 28);
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
    async fn list_memory_returns_keys_matching_prefix() {
        // Post-migration-0035: `list_memory` is no longer a stub.
        // Seed two matching keys and one non-matching, then verify
        // the prefix filter, the read-down trust visibility, and the
        // shape returned to the agent (key + updated_at).
        let db = fresh_db();
        let cid = seed_conversation(&db, "c9");

        // Seed three memory rows directly via the store. The Tool
        // ctx's MemoryApi will be Controller-scoped so all three
        // are readable (all written under Controller).
        let now = 1_700_000_000;
        let store = crate::memory::MemoryStore::new(&db);
        for k in &["pref_voice", "pref_tone", "other"] {
            store
                .upsert(&crate::memory::MemoryEntry {
                    scope: "global".into(),
                    trust_class: "Controller".into(),
                    key: (*k).into(),
                    value_blob: b"v".to_vec(),
                    ttl_expires: None,
                    updated_at: now,
                    tier: crate::memory::MemoryTier::Warm,
                    hits: 0,
                    last_used_at: None,
                    created_at: now,
                })
                .unwrap();
        }

        let tool = ListMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let out = tool
            .invoke(ctx, json!({"scope": "global", "prefix": "pref_"}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                let keys = v["keys"].as_array().expect("keys array");
                let names: std::collections::HashSet<_> = keys
                    .iter()
                    .map(|e| e["key"].as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(names.len(), 2);
                assert!(names.contains("pref_voice"));
                assert!(names.contains("pref_tone"));
                assert!(!names.contains("other"));
                // The "not yet implemented" note from the stub era
                // must be gone — the tool now returns just the keys
                // array, no apologetic disclaimer.
                assert!(v.get("note").is_none());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_memory_excludes_cold_entries() {
        // COLD rows must not surface to the agent through `list_memory`
        // — they exist for audit / explicit lookup only.
        let db = fresh_db();
        let cid = seed_conversation(&db, "c9b");
        let store = crate::memory::MemoryStore::new(&db);
        let now = 1_700_000_000;
        let mut warm = crate::memory::MemoryEntry {
            scope: "global".into(),
            trust_class: "Controller".into(),
            key: "live_pref".into(),
            value_blob: b"v".to_vec(),
            ttl_expires: None,
            updated_at: now,
            tier: crate::memory::MemoryTier::Warm,
            hits: 0,
            last_used_at: None,
            created_at: now,
        };
        store.upsert(&warm).unwrap();
        warm.key = "archived_pref".into();
        warm.tier = crate::memory::MemoryTier::Cold;
        store.upsert(&warm).unwrap();

        let tool = ListMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let out = tool
            .invoke(ctx, json!({"scope": "global", "prefix": ""}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                let keys: Vec<_> = v["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["key"].as_str().unwrap().to_owned())
                    .collect();
                assert!(keys.contains(&"live_pref".to_owned()));
                assert!(!keys.contains(&"archived_pref".to_owned()));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_memory_bumps_hit_counter_and_last_used_at() {
        // Self-improving lifecycle: a successful read must bump the
        // row's `hits` and stamp `last_used_at` so the promotion
        // sweeper can find candidates. This is the read-side half
        // of the frequency-promotion path.
        let db = fresh_db();
        let cid = seed_conversation(&db, "c9c");
        let store = crate::memory::MemoryStore::new(&db);
        let now = 1_700_000_000;
        store
            .upsert(&crate::memory::MemoryEntry {
                scope: "global".into(),
                trust_class: "Controller".into(),
                key: "tracked".into(),
                value_blob: b"v".to_vec(),
                ttl_expires: None,
                updated_at: now,
                tier: crate::memory::MemoryTier::Warm,
                hits: 0,
                last_used_at: None,
                created_at: now,
            })
            .unwrap();

        let tool = ReadMemoryTool::new();
        let ctx = build_ctx(&db, cid, "Controller", false, true);
        let _ = tool
            .invoke(ctx, json!({"scope": "global", "key": "tracked"}))
            .await;

        let row = store
            .get("global", "Controller", "tracked")
            .unwrap()
            .unwrap();
        assert_eq!(row.hits, 1, "successful read must bump hits");
        assert!(row.last_used_at.is_some(), "must stamp last_used_at");
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
    async fn research_status_surfaces_clarification_question_when_awaiting_input() {
        // Reach into the store directly to put the row into
        // awaiting_input — the runner is what does this in production
        // but for the tool-surface test we just need the row in the
        // right state.
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let spawn_ctx = build_ctx_with_research_spawn(&db, cid.clone(), "Controller");
        let started = ResearchStartTool::new()
            .invoke(spawn_ctx, json!({"query": "vague"}))
            .await;
        let job_id = match started {
            ToolOutcome::Ok(v) => v["job"]["id"].as_str().unwrap().to_owned(),
            other => panic!("seed: {other:?}"),
        };
        crate::research::ResearchJobStore::new(&db)
            .set_awaiting_input(
                &crate::ids::ResearchJobId::from(job_id.clone()),
                "Which region?",
                500,
            )
            .unwrap();
        let read_ctx = build_ctx_with_research_read(&db, cid, "Controller");
        let out = ResearchStatusTool::new()
            .invoke(read_ctx, json!({"job_id": job_id}))
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(v["job"]["status"], "awaiting_input");
                assert_eq!(v["job"]["clarification_question"], "Which region?");
                // `error` keeps the same value for backward compat.
                assert_eq!(v["job"]["error"], "Which region?");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_clarify_resumes_awaiting_input_job_with_augmented_query() {
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let started = ResearchStartTool::new()
            .invoke(
                build_ctx_with_research_spawn(&db, cid.clone(), "Controller"),
                json!({"query": "Recommend ground covers."}),
            )
            .await;
        let job_id = match started {
            ToolOutcome::Ok(v) => v["job"]["id"].as_str().unwrap().to_owned(),
            other => panic!("seed: {other:?}"),
        };
        crate::research::ResearchJobStore::new(&db)
            .set_awaiting_input(
                &crate::ids::ResearchJobId::from(job_id.clone()),
                "Which USDA zone?",
                500,
            )
            .unwrap();

        let out = ResearchClarifyTool::new()
            .invoke(
                build_ctx_with_research_spawn(&db, cid.clone(), "Controller"),
                json!({"job_id": job_id, "clarification": "Zone 6, Pacific NW."}),
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert_eq!(
                    v["job"]["status"], "pending",
                    "should re-enter pending queue"
                );
                let q = v["job"]["query"].as_str().unwrap();
                assert!(q.contains("ground covers"), "original query preserved");
                assert!(q.contains("Zone 6"), "clarification appended");
                // The old question must be cleared.
                assert!(v["job"]["clarification_question"].is_null());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_clarify_denied_when_capability_missing() {
        // Adversarial: a read-only ctx must not be able to clarify.
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let read_ctx = build_ctx_with_research_read(&db, cid, "Controller");
        let out = ResearchClarifyTool::new()
            .invoke(read_ctx, json!({"job_id": "x", "clarification": "y"}))
            .await;
        match out {
            ToolOutcome::Denied { .. } => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn research_clarify_returns_not_found_for_pending_or_terminal_job() {
        // Calling clarify on a row that is not in awaiting_input must
        // be a clean error — never silently overwrite.
        let db = fresh_db();
        let cid = seed_conversation(&db, "c1");
        let started = ResearchStartTool::new()
            .invoke(
                build_ctx_with_research_spawn(&db, cid.clone(), "Controller"),
                json!({"query": "q"}),
            )
            .await;
        let job_id = match started {
            ToolOutcome::Ok(v) => v["job"]["id"].as_str().unwrap().to_owned(),
            other => panic!("seed: {other:?}"),
        };
        // Row is still Pending — clarify must reject.
        let out = ResearchClarifyTool::new()
            .invoke(
                build_ctx_with_research_spawn(&db, cid, "Controller"),
                json!({"job_id": job_id, "clarification": "answer"}),
            )
            .await;
        match out {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "not_found"),
            other => panic!("expected Err(not_found), got {other:?}"),
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
