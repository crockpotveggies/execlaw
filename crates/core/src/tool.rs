//! Tool-implementation trait + capability-scoped runtime context.
//!
//! This is the contract every executable tool — built-in, plugin, MCP —
//! eventually satisfies. The runtime hands a tool an `invoke(ctx, args)`
//! call where `ctx` exposes ONLY the capability APIs the tool's
//! descriptor declared. There is intentionally **no `db: Database`
//! handle** in `ToolCtx`: a tool that wants to read messages calls
//! `ctx.conversation.read_events(...)` and gets a trust-filtered view;
//! a tool that wants to read memory calls `ctx.memory.read(...)` and
//! gets a trust-class-scoped lookup. The implementations of those API
//! traits hold the underlying `Database` privately.
//!
//! ## Why no raw DB handle
//!
//! Passing a `Database` to a tool would let any tool — built-in or
//! plugin — read every table: users, refresh tokens, vault blobs,
//! every conversation's events. That's ambient authority and a textbook
//! prompt-injection blast-radius problem. The capability-scoped APIs
//! are the security boundary: a tool can only do what its declared
//! capability set allows, the implementation enforces caller-trust and
//! caller-conversation scoping internally, and a buggy tool's worst
//! case is bounded by what its capabilities expose.
//!
//! ## Where this fits
//!
//! - `ToolImpl` — the trait every tool implements. Stateless; one
//!   `Arc<dyn ToolImpl>` per tool, registered into the host's
//!   `HookRegistry`. The dispatch layer constructs a `ToolCtx` per call.
//! - `ToolDescriptor` — the metadata the registry stores: name, schema,
//!   capability set, default allowed trust classes. The LLM sees the
//!   `name` + `description` + `schema`.
//! - `Capability` — the enum a descriptor lists. Each variant maps 1:1
//!   to an `Option<Arc<dyn FooApi>>` field on `ToolCtx`. The runtime
//!   populates a field if and only if the descriptor declared that
//!   capability — so a tool that didn't declare `WebFetch` finds
//!   `ctx.web_fetch == None` and can't even construct a call.
//! - Capability API traits (`ConversationApi`, `MemoryApi`, …) — narrow
//!   façades over the underlying stores. Methods take only what's
//!   intrinsic to the operation (e.g. `read_events(before, limit)`)
//!   because caller-trust and caller-conversation are baked into the
//!   trait-object impl at construction time.
//!
//! 2026-04-29.

use crate::ids::ConversationId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

// -----------------------------------------------------------------
// Descriptor + invocation contract
// -----------------------------------------------------------------

/// Latency hint the LLM (and the SPA's tool list) reads to decide
/// when calling the tool feels appropriate. Hard timeouts are
/// enforced separately by the dispatch layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ToolLatency {
    /// <100 ms expected — pure DB read, in-memory transform.
    Low,
    /// <1 s expected — local network call, small file read.
    Medium,
    /// Multi-second — outbound API, model call, long aggregation.
    High,
}

impl ToolLatency {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// Where the tool's implementation lives. The Settings → Tools page
/// uses this to badge rows; the dispatch layer treats all sources
/// uniformly once the access gate has cleared the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ToolSource {
    /// Compiled into the binary. Registered at server boot.
    Builtin,
    /// Lives in a plugin subprocess. Registered at plugin install.
    Plugin,
    /// Reflected from a connected MCP server.
    Mcp,
}

impl ToolSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Plugin => "plugin",
            Self::Mcp => "mcp",
        }
    }
}

/// Capability requested by a tool, populated as a corresponding
/// `Option<Arc<dyn FooApi>>` field on `ToolCtx` when the runtime
/// builds the context for an invocation.
///
/// New capabilities get added here when new tool families land — a
/// closed enum keeps the runtime/tool contract grep-able and prevents
/// plugins from inventing capabilities that the host doesn't actually
/// have a corresponding API for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read this conversation's metadata (display name, kind, phase).
    ConversationRead,
    /// Mutate this conversation's metadata. Set thread name, etc.
    ConversationWrite,
    /// Read memory entries at the caller's trust class or below.
    MemoryRead,
    /// Write memory entries — always at the caller's own trust class.
    MemoryWrite,
    /// Read scheduled tasks visible to the caller.
    TaskRead,
    /// Create / mutate scheduled tasks.
    TaskWrite,
    /// Read recurring schedules / routines.
    ScheduleRead,
    /// Create / mutate recurring schedules / routines.
    ScheduleWrite,
    /// Outbound HTTP GET, allowlist-gated.
    WebFetch,
    /// Web search via the configured provider.
    Search,
    /// Send a notification to the controller (alerts / sideband).
    Notify,
    /// Spawn a sub-agent / delegated turn.
    SubagentSpawn,
    /// Enqueue a deep-research job. Distinct from `SubagentSpawn`
    /// because research is a long-running, persistent, operator-
    /// visible job — not a synchronous in-turn child call.
    ResearchSpawn,
    /// Read research-job rows + their plan / progress / report.
    /// Splits from `ResearchSpawn` so a low-trust caller can poll
    /// status on a job an operator started without being able to
    /// start new ones.
    ResearchRead,
    /// Deliver an attachment (file) to the conversation. Symmetric
    /// to channel plugins' `send_attachment` — gives the agent a
    /// way to formally attach a file (e.g. a research PDF) to its
    /// reply on the web channel. The capability is gated separately
    /// from `Notify` because it only makes sense with a registered
    /// AttachmentRow and a meaningful conversation_id.
    AttachmentSend,
    /// Outbound dispatch through a bridged transport (Signal,
    /// WhatsApp, Matrix, ...). The runtime hands the tool a
    /// `TransportApi` keyed by `channel`; resolving recipients,
    /// sending messages, and reading the current inbound chat id
    /// all flow through it. Distinct from `Notify` (which is the
    /// controller's priority-channel sideband — typically a
    /// single, hard-wired transport configured by the operator),
    /// because `Transport` is the agent's general "send outbound
    /// on a registered bridge" surface.
    Transport,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConversationRead => "conversation_read",
            Self::ConversationWrite => "conversation_write",
            Self::MemoryRead => "memory_read",
            Self::MemoryWrite => "memory_write",
            Self::TaskRead => "task_read",
            Self::TaskWrite => "task_write",
            Self::ScheduleRead => "schedule_read",
            Self::ScheduleWrite => "schedule_write",
            Self::WebFetch => "web_fetch",
            Self::Search => "search",
            Self::Notify => "notify",
            Self::SubagentSpawn => "subagent_spawn",
            Self::ResearchSpawn => "research_spawn",
            Self::ResearchRead => "research_read",
            Self::AttachmentSend => "attachment_send",
            Self::Transport => "transport",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "conversation_read" => Some(Self::ConversationRead),
            "conversation_write" => Some(Self::ConversationWrite),
            "memory_read" => Some(Self::MemoryRead),
            "memory_write" => Some(Self::MemoryWrite),
            "task_read" => Some(Self::TaskRead),
            "task_write" => Some(Self::TaskWrite),
            "schedule_read" => Some(Self::ScheduleRead),
            "schedule_write" => Some(Self::ScheduleWrite),
            "web_fetch" => Some(Self::WebFetch),
            "search" => Some(Self::Search),
            "notify" => Some(Self::Notify),
            "subagent_spawn" => Some(Self::SubagentSpawn),
            "research_spawn" => Some(Self::ResearchSpawn),
            "research_read" => Some(Self::ResearchRead),
            "attachment_send" => Some(Self::AttachmentSend),
            "transport" => Some(Self::Transport),
            _ => None,
        }
    }
}

/// Static metadata for a single tool. Cheap to clone (every field
/// except `schema` is a small string or enum, and `schema` is wrapped
/// in `Arc` inside `Value` after the first parse).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Globally-unique name. The LLM uses this in its tool-call
    /// payload; the dispatch layer keys on it.
    pub name: String,
    /// One- or two-sentence description shown to the LLM.
    pub description: String,
    /// JSON Schema for the args the LLM is expected to send.
    pub schema: Value,
    /// Where the implementation lives.
    pub source: ToolSource,
    /// Latency hint shown to the LLM and the operator.
    pub latency: ToolLatency,
    /// Capabilities the tool needs. The runtime populates the
    /// corresponding `ToolCtx` field for each — if you don't declare
    /// `MemoryRead`, `ctx.memory` is `None` at invocation time.
    pub capabilities: Vec<Capability>,
    /// Default trust classes that should be allowed when this tool's
    /// `config_tool_access` row is first inserted. Operator overrides
    /// via Settings persist; the seed only fires on first sight.
    pub default_allowed_classes: Vec<String>,
}

/// Outcome of a `ToolImpl::invoke` call. `Denied` is distinct from
/// `Err` so the dispatch layer can surface "the tool exists but the
/// runtime/capability layer refused the call" without conflating it
/// with a runtime error inside the tool body.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    /// The tool ran to completion and produced a JSON value to return
    /// to the model.
    Ok(Value),
    /// The tool body errored. `code` is a stable short string for
    /// programmatic handling; `message` is human-facing.
    Err { code: String, message: String },
    /// The dispatch refused the call (capability mismatch, trust gate,
    /// approval rejection). Bubbles to the model as a structured
    /// `tool_result` with `denied = true` so it can self-correct.
    Denied { reason: String },
}

impl ToolOutcome {
    pub fn ok(v: Value) -> Self {
        Self::Ok(v)
    }
    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Err {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }
}

/// The trait every tool implements. Stateless: one `Arc<dyn ToolImpl>`
/// per tool, kept in the registry, called concurrently across turns.
#[async_trait]
pub trait ToolImpl: Send + Sync + 'static {
    /// Static metadata. The registry caches this; the LLM-facing
    /// catalog is built from it; the access gate compares the caller's
    /// trust class against `default_allowed_classes` (on first install).
    fn descriptor(&self) -> &ToolDescriptor;

    /// Run the tool. The `ctx` carries only the capability APIs the
    /// descriptor declared; the args are the raw JSON the LLM sent.
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome;
}

// -----------------------------------------------------------------
// Capability-scoped runtime context
// -----------------------------------------------------------------

/// Per-call runtime context handed to `ToolImpl::invoke`. The presence
/// of each `Option<Arc<dyn FooApi>>` field tracks the descriptor's
/// declared `Capability` set: declared → `Some`, undeclared → `None`.
///
/// `ToolCtx` is intentionally NOT `Clone` — a tool gets one per call.
/// It IS cheap to construct (each field is a refcount bump) so the
/// dispatcher creates fresh instances rather than sharing.
pub struct ToolCtx {
    /// The conversation this turn belongs to. Used by capability impls
    /// to scope their reads/writes; tools should treat this as
    /// read-only metadata.
    pub conversation_id: ConversationId,
    /// Caller's trust class as a string. Stays as `String` here so
    /// `core` doesn't depend on `execlaw-policy`'s `TrustLevel` enum;
    /// the capability impls parse it as needed.
    pub caller_trust: String,
    /// Wall-clock provider, abstracted for tests.
    pub clock: Arc<dyn Clock>,

    /// Capability APIs. Populated iff the tool's descriptor declared
    /// the corresponding `Capability`. A tool that did NOT declare
    /// `MemoryRead`/`MemoryWrite` finds `memory == None`.
    pub conversation: Option<Arc<dyn ConversationApi>>,
    pub memory: Option<Arc<dyn MemoryApi>>,
    pub notify: Option<Arc<dyn NotifyApi>>,
    pub schedule: Option<Arc<dyn ScheduleApi>>,
    pub web_fetch: Option<Arc<dyn WebFetchApi>>,
    pub search: Option<Arc<dyn WebSearchApi>>,
    pub subagent: Option<Arc<dyn SubagentApi>>,
    pub research: Option<Arc<dyn ResearchApi>>,
    pub attachments: Option<Arc<dyn AttachmentApi>>,
    /// Outbound bridge transport. Populated for tools that declared
    /// `Capability::Transport`. Consumers pass `channel` ("signal",
    /// "whatsapp", ...) on each call so one capability slot serves
    /// every registered bridge — no per-channel cap proliferation.
    pub transport: Option<Arc<dyn TransportApi>>,
}

impl ToolCtx {
    /// Construct a context with no capabilities populated. Tests use
    /// this and then attach the specific APIs they want; production
    /// code uses the dispatch-layer builder.
    pub fn empty(
        conversation_id: ConversationId,
        caller_trust: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            conversation_id,
            caller_trust: caller_trust.into(),
            clock,
            conversation: None,
            memory: None,
            notify: None,
            schedule: None,
            web_fetch: None,
            search: None,
            subagent: None,
            research: None,
            attachments: None,
            transport: None,
        }
    }
}

/// Wall-clock abstraction. Production code uses `SystemClock`; tests
/// use a fixed-time stub.
pub trait Clock: Send + Sync + 'static {
    /// Unix-seconds wall clock.
    fn now_unix(&self) -> i64;
}

/// Default `Clock` impl using `chrono::Utc::now()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

// -----------------------------------------------------------------
// Capability API traits
// -----------------------------------------------------------------

/// Errors a capability API can return. Each variant maps to a
/// `ToolOutcome` flavor at the dispatch layer.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid argument: {0}")]
    Validation(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl ApiError {
    /// Convert this error into a `ToolOutcome` for tool implementations
    /// that want to surface it as a tool error rather than handle it
    /// specifically.
    pub fn into_outcome(self) -> ToolOutcome {
        match self {
            Self::NotAuthorized(reason) => ToolOutcome::Denied { reason },
            Self::NotFound(s) => ToolOutcome::Err {
                code: "not_found".into(),
                message: s,
            },
            Self::Validation(s) => ToolOutcome::Err {
                code: "invalid_argument".into(),
                message: s,
            },
            Self::Storage(s) => ToolOutcome::Err {
                code: "storage_error".into(),
                message: s,
            },
        }
    }
}

/// Read-only thread / conversation metadata visible to the caller.
/// Mutations go through `ConversationApi::set_thread_name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub conversation_id: String,
    pub display_name: Option<String>,
}

/// Compact thread summary returned by `ConversationApi::list_threads`.
/// Mirrors the SPA sidebar's projection so the LLM-facing list and
/// the operator-facing list stay in sync.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadListEntry {
    pub conversation_id: String,
    pub display_name: Option<String>,
    pub trust_class: String,
    pub is_pinned: bool,
    /// Most recent activity (Unix-seconds). Lets the LLM sort
    /// "what's been going on lately" without an extra round-trip.
    pub last_activity_at: i64,
}

/// One entry in the chat-history view returned by
/// [`ConversationApi::read_history`]. Internal/operational events
/// (phase markers, voice frames, alerts) are filtered out at the
/// implementation layer — only the events the LLM legitimately
/// wants to see when reading "what was just said" are surfaced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Monotonic per-conversation sequence number.
    pub seq: i64,
    /// Either `"user"` (a controller / contact message) or
    /// `"agent"` (a model reply). `tool_use` / `tool_result` are
    /// excluded from this view; they get their own surface in a
    /// future tool.
    pub role: String,
    /// Raw transcript text as committed to the event. Empty string
    /// when the payload didn't carry one (rare; defensive).
    pub text: String,
    /// Unix-seconds when the event committed to the log.
    pub committed_at: i64,
}

/// Conversation-scoped API. The implementation captures the caller's
/// `conversation_id` + `caller_trust` at construction; method args
/// don't take them, so a tool can't "see" another conversation by
/// passing a different id.
#[async_trait]
pub trait ConversationApi: Send + Sync {
    /// Get thread metadata (display name, etc.) for the caller's
    /// conversation.
    async fn get_thread(&self) -> Result<ThreadInfo, ApiError>;
    /// Set the human-readable display name for the caller's
    /// conversation. Trimmed; empty rejected.
    async fn set_thread_name(&self, name: &str) -> Result<(), ApiError>;
    /// Read the most recent N user / model events from the caller's
    /// conversation, optionally paginated by `before_seq` (returns
    /// events with `seq < before_seq`). Results are ordered newest-
    /// first so the LLM can read them top-to-bottom for "what just
    /// happened" context. `limit` is capped by the implementation
    /// to prevent runaway window sizes.
    async fn read_history(
        &self,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, ApiError>;

    /// List threads visible to the caller. Phase 1: returns every
    /// non-ephemeral thread. Future trust-class scoping (a
    /// `KnownTrusted` caller only seeing their own threads) lands in
    /// a follow-up that goes alongside the principal-graph work.
    async fn list_threads(&self) -> Result<Vec<ThreadListEntry>, ApiError>;
}

/// One memory key + its update timestamp. Returned by
/// `MemoryApi::list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryListEntry {
    pub key: String,
    pub updated_at: i64,
}

/// Severity hint a tool sends to the operator's notification surface.
/// `Info` is the safe default — agents shouldn't fire `Critical`
/// without explicit operator framing because Critical alerts can
/// route to phone-call escalation in some operator setups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum NotifySeverity {
    Info,
    Warning,
    Error,
    Critical,
}

impl NotifySeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Error => "Error",
            Self::Critical => "Critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Info" => Some(Self::Info),
            "Warning" => Some(Self::Warning),
            "Error" => Some(Self::Error),
            "Critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Notification handle a tool was issued to send a message to the
/// controller. Implementations route through whatever sideband
/// surface the operator configured (UI dropdown, Signal, alert pill);
/// this trait abstracts the actual transport so the tool body only
/// has to care about what it wants to say.
#[async_trait]
pub trait NotifyApi: Send + Sync {
    /// Send a notification to the controller. `title` is shown
    /// prominently; `detail` is optional longer-form text.
    /// Notifications are deduped by fingerprint so the same agent
    /// shouting the same thing repeatedly doesn't drown the operator.
    async fn notify(
        &self,
        severity: NotifySeverity,
        title: &str,
        detail: Option<&str>,
    ) -> Result<NotifyReceipt, ApiError>;
}

/// Returned by [`NotifyApi::notify`] so the tool can echo a
/// stable handle back to the LLM (useful for ack-tracking later).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyReceipt {
    /// Stable per-(conversation,fingerprint) alert id.
    pub alert_id: String,
    /// Whether this notification deduplicated against an existing
    /// firing alert (`true`) or opened a new one (`false`).
    pub deduplicated: bool,
}

/// Compact summary of a scheduled-task / routine row, suitable for
/// returning to the LLM. Drops bookkeeping fields the agent doesn't
/// need (created_at, updated_at) but keeps every field operators
/// would want to inspect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutineSummary {
    pub id: String,
    pub name: String,
    pub schedule_cron: String,
    pub timezone: String,
    pub prompt: String,
    pub target_conversation_id: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_run_status: Option<String>,
    pub next_run_at: Option<i64>,
}

/// Schedule API: backed by the `RoutineStore`. The implementation
/// captures the caller's `caller_trust` at construction so it can
/// reject privileged writes (e.g. cross-conversation scheduling)
/// from low-trust callers.
#[async_trait]
pub trait ScheduleApi: Send + Sync {
    /// Create a recurring task. Returns the new routine's id.
    async fn create_routine(
        &self,
        name: &str,
        schedule_cron: &str,
        prompt: &str,
        target_conversation_id: Option<&str>,
        timezone: Option<&str>,
    ) -> Result<RoutineSummary, ApiError>;

    /// List every routine visible to the caller. Phase 1 returns all
    /// routines; future trust-class scoping (a `KnownTrusted` caller
    /// only seeing their own thread's routines) lands in a follow-up.
    async fn list_routines(&self) -> Result<Vec<RoutineSummary>, ApiError>;

    /// Look up a single routine by id. `None` if no such routine.
    async fn get_routine(&self, id: &str) -> Result<Option<RoutineSummary>, ApiError>;

    /// Update an existing routine. All fields besides `id` are
    /// optional — pass `None` to keep the existing value.
    async fn update_routine(
        &self,
        id: &str,
        name: Option<&str>,
        schedule_cron: Option<&str>,
        prompt: Option<&str>,
        target_conversation_id: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<RoutineSummary, ApiError>;

    /// Toggle a routine's `enabled` flag. Returns the updated row.
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<RoutineSummary, ApiError>;

    /// Delete a routine permanently. Returns `true` if a row was
    /// actually deleted, `false` if no routine matched the id.
    async fn delete_routine(&self, id: &str) -> Result<bool, ApiError>;
}

/// Result of a `WebFetchApi::get` — the response body capped at the
/// implementation's size limit + the resolved final URL (in case of
/// redirects) + the content-type the server returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchResponse {
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    /// UTF-8 lossily decoded body, truncated at the implementation's
    /// size cap. Binary content (image/*, video/*, etc.) is rejected
    /// before this point.
    pub body: String,
    /// `true` when the body was truncated at the size cap. The LLM
    /// can decide whether to ask for the rest with a `range` arg in
    /// a future iteration.
    pub truncated: bool,
}

/// One search result returned by [`WebSearchApi::search`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
}

/// Web-search capability backed by an operator-configured provider.
///
/// The trait is provider-agnostic by design: the implementation
/// contains the per-vendor request shape (DuckDuckGo HTML scrape,
/// Brave Search API, Exa, Tavily, Kagi, SearxNG, etc.) and the
/// dispatcher swaps in whichever provider the operator chose in
/// Settings → Search. Tools see only this trait surface.
#[async_trait]
pub trait WebSearchApi: Send + Sync {
    /// Run a web search. `max_results` is a hint — the implementation
    /// caps it to whatever the underlying provider's free-tier or
    /// fair-use limit allows. Returns `Ok(empty)` (with a clear
    /// metadata note in the tool wrapper) when no provider is
    /// configured, rather than erroring — so the catalog stays
    /// stable across operator setups.
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError>;

    /// Identifier for the provider that ran this search (e.g.
    /// `"duckduckgo"`, `"brave"`). Surfaced to the tool result so the
    /// LLM can know which vendor answered.
    fn provider_id(&self) -> &str;
}

/// One subagent turn — a child LLM call the parent agent fires off
/// for a bounded sub-task. Synchronous: the parent's turn blocks
/// until the subagent returns. Use [`crate::cards`] + a job runner
/// for async work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// The sub-task prompt — what the parent wants the child to do.
    pub task: String,
    /// Optional context the parent attaches verbatim ahead of the
    /// task prompt (relevant excerpts, prior decisions, etc.).
    pub context: Option<String>,
    /// Token budget for the child's reply. Implementations cap to a
    /// reasonable upper bound regardless.
    pub max_tokens: Option<u32>,
}

/// Result of a [`SubagentApi::delegate`] call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResponse {
    /// Plain text the child produced.
    pub text: String,
    /// Stable id the runtime emits to the WS event bus alongside
    /// `SubagentStarted` / `SubagentCompleted`. Lets the SPA's
    /// typing-indicator pill show "subagent: <task>" while the
    /// parent waits.
    pub task_id: String,
    /// Tokens consumed (best-effort — the inference backend's
    /// usage block may be missing).
    pub tokens_used: Option<u32>,
}

/// Subagent-spawn capability. Implementation makes a child LLM call
/// against the parent's inference backend with a focused prompt
/// (system prompt + optional context + task), waits for the
/// completion, and returns the text. Synchronous; the parent's
/// turn is paused for the duration.
///
/// For async / multi-minute work that the operator should be able
/// to inspect mid-flight, use a job-system path (see
/// `crate::cards`) instead — subagents are NOT the right primitive
/// for long-running tasks.
#[async_trait]
pub trait SubagentApi: Send + Sync {
    async fn delegate(&self, req: &SubagentRequest) -> Result<SubagentResponse, ApiError>;
}

/// Compact projection of a single research-job row, suitable for
/// returning to the LLM. Mirrors `core::research::ResearchJobSummary`
/// but lives here so the trait surface stays in `core::tool` (the
/// implementation in `core::tool_apis` does the conversion).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResearchJobView {
    pub id: String,
    pub conversation_id: String,
    pub query: String,
    pub status: String,
    pub card_id: Option<String>,
    pub workspace_path: Option<String>,
    pub attachment_id: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Decoded plan if present. JSON value (rather than the typed
    /// shape) so the ResearchApi caller doesn't need a hard
    /// dependency on `core::research::ResearchPlan`.
    pub plan: Option<Value>,
}

/// Deep-research capability. Implementations write to
/// `state_research_jobs` (start) and read it (status / list); the
/// runner actor (server-side) picks Pending rows up and drives the
/// pipeline.
///
/// Splitting `start` from `status` + `list` mirrors the
/// `ResearchSpawn` / `ResearchRead` capability split — a tool that
/// only declares `ResearchRead` will find `ctx.research`'s `start`
/// path returns `NotAuthorized` because the impl was constructed
/// in read-only mode.
#[async_trait]
pub trait ResearchApi: Send + Sync {
    /// Enqueue a new research job for the caller's conversation.
    /// Returns the inserted row's id and its initial summary.
    /// Implementations that were constructed without spawn
    /// authority must return `ApiError::NotAuthorized`.
    async fn start(
        &self,
        query: &str,
        overrides_json: Option<Vec<u8>>,
    ) -> Result<ResearchJobView, ApiError>;

    /// Look up a single job by id. `None` when no such job. Trust
    /// scoping: a caller below `Controller` only sees jobs belonging
    /// to their own conversation; the impl returns `Ok(None)` rather
    /// than `NotAuthorized` so probing for cross-thread ids leaks no
    /// information.
    async fn status(&self, job_id: &str) -> Result<Option<ResearchJobView>, ApiError>;

    /// List jobs visible to the caller. Below-Controller callers see
    /// only their own conversation; Controllers see every row.
    async fn list(&self) -> Result<Vec<ResearchJobView>, ApiError>;

    /// Read the synthesized report for a completed job. Returns
    /// `Ok(None)` when the job exists but has no report yet (still
    /// gathering / synthesizing / failed) or when the job isn't
    /// visible to the caller. Trust scoping mirrors `status` —
    /// below-Controller callers see only their own conversation.
    async fn get_report(&self, job_id: &str) -> Result<Option<String>, ApiError>;

    /// Resume an `awaiting_input` job by feeding back the operator's
    /// clarification. The runner appended a clarification question
    /// to the job's `error` field when the planner judged the
    /// original query too vague; the agent surfaces that question
    /// to the user in chat (it is the primary interface) and calls
    /// this method with their answer. The implementation augments
    /// the job's stored query with the clarification and resets
    /// status to `pending` so the supervisor re-claims it on the
    /// next tick.
    ///
    /// Returns the updated `ResearchJobView`. Errors:
    ///   * `ApiError::NotAuthorized` if the impl was constructed
    ///     without spawn authority (clarify is a write).
    ///   * `ApiError::NotFound` if the job id is not visible to the
    ///     caller OR is not currently in `awaiting_input` (the
    ///     `awaiting_input` guard prevents double-resume races and
    ///     resuming a Cancelled row).
    async fn clarify(&self, job_id: &str, clarification: &str)
    -> Result<ResearchJobView, ApiError>;
}

/// Caller-facing summary of an attachment after `send_attachment`
/// has emitted it into the conversation. Returned to the agent so
/// it can confirm delivery to the user (and reference the
/// `download_url` in its prose if it wants to).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredAttachmentView {
    pub attachment_id: String,
    pub conversation_id: String,
    pub mime_type: String,
    pub filename: String,
    pub byte_size: Option<i64>,
    /// Web-channel download URL (e.g. `/api/attachments/<id>`).
    /// The SPA renders the inline chip from the emitted card; this
    /// URL is for the agent's own reply prose if it wants a
    /// link-style affordance.
    pub download_url: String,
    /// Optional one-line caption the agent attached to the file.
    pub caption: Option<String>,
}

/// Send-an-attachment-to-the-conversation capability. Symmetric to
/// channel plugins' `send_attachment` (which delivers a file over a
/// TextOnly transport like SMS / email); this trait is the
/// web-channel equivalent — emits an `Attachment` card into the
/// conversation that the SPA renders inline as a download chip.
///
/// Trust scoping:
///   * The implementation refuses to deliver an attachment whose
///     stored `conversation_id` doesn't match the caller's
///     conversation. Returns `ApiError::NotFound` (not
///     NotAuthorized) so cross-thread id probing is opaque.
///   * The capability is gated by `Capability::AttachmentSend`;
///     the dispatch layer constructs `ctx.attachments = None` for
///     callers that didn't declare it.
#[async_trait]
pub trait AttachmentApi: Send + Sync {
    /// Deliver `attachment_id` to the caller's conversation as an
    /// inline chip with optional `caption`. Returns the rendered
    /// view the agent can reference in its prose.
    async fn send(
        &self,
        attachment_id: &str,
        caption: Option<&str>,
    ) -> Result<DeliveredAttachmentView, ApiError>;
}

/// Outbound HTTP capability. Implementations are responsible for SSRF
/// defenses (reject private-network targets, file:// URLs, etc.) and
/// for enforcing the operator's domain allowlist if one is configured.
#[async_trait]
pub trait WebFetchApi: Send + Sync {
    /// Fetch a URL via HTTP GET. The implementation enforces:
    ///   * scheme must be http or https
    ///   * resolved IP must not be in private / loopback / link-local
    ///     ranges (SSRF guard)
    ///   * content-type must be text-y (text/*, application/json,
    ///     application/xml; rejects binary)
    ///   * response size capped (default 1 MiB; truncated body is
    ///     flagged in the response)
    ///   * 30s timeout
    async fn get(&self, url: &str) -> Result<WebFetchResponse, ApiError>;
}

/// Outbound transport / bridge API.
///
/// Mirrors selfhosted-claw's per-turn `ctx.sendMessage` /
/// `ctx.resolveRecipient` / `ctx.chatJid` triple, but parameterised
/// by `channel` so a single capability slot can serve every bridged
/// transport. Bridge-supervised plugins (Signal, WhatsApp, ...) all
/// land here; the supervisor dispatches by channel string to the
/// right RPC handle.
///
/// Errors come back as `ApiError`. Bridge-specific failure modes
/// (signal-cli unreachable, recipient blocked the controller, group
/// membership lost, ...) all collapse to `ApiError` variants — the
/// tool's job is to convert to a user-facing `ToolOutcome::Err` with
/// a stable code.
#[async_trait]
pub trait TransportApi: Send + Sync {
    /// Resolve a free-form recipient string (display name, phone
    /// number, foreign id, ...) to a canonical foreign id the
    /// bridge can dispatch to. Implementations consult the contacts
    /// registry first, then fall back to the bridge's own contact
    /// list (signal-cli has one, signal-cli's `--contact` lookup).
    /// Returns `ApiError::NotFound` when nothing matches.
    async fn resolve_recipient(&self, channel: &str, free_form: &str) -> Result<String, ApiError>;

    /// Dispatch `text` to `recipient` (a canonical foreign id).
    /// Returns the bridge's message id for read-receipt /
    /// idempotency correlation; the supervisor logs it.
    async fn send(&self, channel: &str, recipient: &str, text: &str) -> Result<String, ApiError>;

    /// The current turn's inbound foreign id, if the turn was
    /// triggered by an inbound message on this transport. Mirror of
    /// selfhosted-claw's `ctx.chatJid`. The `signal.reply` tool
    /// reads this and dispatches without taking a recipient arg.
    /// `Ok(None)` when the turn was triggered some other way (web
    /// SPA, scheduled task, controller-initiated, ...).
    ///
    /// Async deliberately — even when an impl caches the value in a
    /// pre-populated `HashMap` (the common case), keeping the
    /// signature async lets us swap to a DB-backed lookup later
    /// without forcing every consumer to migrate. The cost of the
    /// extra `await` on a synchronous-cache hit is one Rust function
    /// call.
    async fn current_chat_id(&self, channel: &str) -> Result<Option<String>, ApiError>;

    // ---- Group operations (Phase 5) -------------------------------
    //
    // Default impls return `ApiError::Validation("not supported")` so
    // a transport that doesn't support groups (email, voice, sms in
    // the future) can stay on three methods. Signal overrides every
    // method below with real signal-cli-rest-api calls.
    //
    // The contract is generic across bridges that have a group
    // concept: each method takes a `channel` so a future
    // multi-transport registry can route by channel internally.

    /// List the controller's groups on this channel. Returns one
    /// [`TransportGroupSummary`] per group the bridge sees the
    /// controller as a member of, in bridge-defined order. Empty
    /// vec when the controller is in no groups; never `Err` for the
    /// "no groups" case.
    async fn list_groups(&self, channel: &str) -> Result<Vec<TransportGroupSummary>, ApiError> {
        let _ = channel;
        Err(ApiError::Validation(
            "list_groups not supported by this transport".into(),
        ))
    }

    /// Create a fresh group on this channel with `members` (each a
    /// canonical foreign id — the caller is responsible for
    /// resolving display-name / phone-number args via
    /// `resolve_recipient` first). `title` is optional; bridges
    /// without titled groups ignore it. Returns the bridge's
    /// canonical group id (Signal: base64 — same value the binding
    /// store uses as `foreign_id`).
    async fn create_group(
        &self,
        channel: &str,
        title: Option<&str>,
        members: &[String],
    ) -> Result<String, ApiError> {
        let _ = (channel, title, members);
        Err(ApiError::Validation(
            "create_group not supported by this transport".into(),
        ))
    }

    /// Add `members` (canonical foreign ids) to an existing group.
    /// `group_id` is the bridge's canonical group id (Signal:
    /// base64). No return value; errors surface bridge-side
    /// permission failures (e.g. controller isn't an admin).
    async fn add_group_members(
        &self,
        channel: &str,
        group_id: &str,
        members: &[String],
    ) -> Result<(), ApiError> {
        let _ = (channel, group_id, members);
        Err(ApiError::Validation(
            "add_group_members not supported by this transport".into(),
        ))
    }

    /// Leave the named group on this channel. The controller's
    /// member row in the bridge's group is removed; the binding row
    /// in `state_transport_bindings` is purged by the caller (the
    /// tool body does it after the bridge call succeeds).
    async fn leave_group(&self, channel: &str, group_id: &str) -> Result<(), ApiError> {
        let _ = (channel, group_id);
        Err(ApiError::Validation(
            "leave_group not supported by this transport".into(),
        ))
    }

    /// Phase 6 — fetch an inbound attachment blob from the bridge
    /// by its bridge-side attachment id (which the inbound envelope
    /// surfaced on `dataMessage.attachments[].id` for signal-cli).
    ///
    /// `attachment_id` is opaque to the host — every bridge owns its
    /// own id space. Implementations stream the response into memory
    /// and return the full bytes; per-attachment size limits are
    /// enforced by the caller (the inbound consumer rejects blobs
    /// over `MAX_INBOUND_ATTACHMENT_BYTES` to keep a malicious
    /// contact from exhausting disk).
    ///
    /// Default impl errors so a non-attachment-aware bridge can
    /// stay on the slimmer trait surface.
    async fn fetch_attachment(
        &self,
        channel: &str,
        attachment_id: &str,
    ) -> Result<FetchedAttachment, ApiError> {
        let _ = (channel, attachment_id);
        Err(ApiError::Validation(
            "fetch_attachment not supported by this transport".into(),
        ))
    }

    /// Phase 7 — outbound dispatch with attachments. `recipient` is
    /// a canonical foreign id (Signal: phone number or
    /// base64-shaped group id); `attachments` is the list of
    /// `state_attachments` ids the message should carry.
    ///
    /// Implementations are responsible for **conversation-scope
    /// validation**: each `attachment_id` must belong to a row
    /// whose `conversation_id` matches the calling turn's
    /// conversation. A miss returns `ApiError::NotFound` (not
    /// `NotAuthorized`) so cross-conversation id probing leaks no
    /// info — same defense pattern as `AttachmentApi::send`.
    ///
    /// Returns the bridge's message id, same shape as
    /// [`Self::send`]. Default impl errors with `Validation` so a
    /// non-attachment-aware bridge stays on the simpler send path.
    async fn send_with_attachments(
        &self,
        channel: &str,
        recipient: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<String, ApiError> {
        let _ = (channel, recipient, text, attachments);
        Err(ApiError::Validation(
            "send_with_attachments not supported by this transport".into(),
        ))
    }

    /// Send a "typing started" indicator to `recipient`. Most chat
    /// transports auto-clear the indicator after a few seconds at
    /// the protocol level (Signal: ~5 seconds; Slack: ~5 seconds);
    /// callers that want a sustained "typing" state during a long
    /// turn should refresh on a cadence shorter than the protocol
    /// timeout. The host wraps this in [`TypingIndicatorGuard`] in
    /// `chats.rs` for the agent-turn use case.
    ///
    /// Default impl is a silent no-op so transports that don't
    /// support typing (email, voice, future bridges) don't have to
    /// override. Errors are surfaced as `ApiError` for the rare
    /// transport-side failure (sidecar crashed mid-call); the
    /// agent-turn caller treats failures as best-effort and
    /// continues.
    async fn start_typing(&self, channel: &str, recipient: &str) -> Result<(), ApiError> {
        let _ = (channel, recipient);
        Ok(())
    }

    /// Send a "typing stopped" indicator. Some protocols emit an
    /// explicit stop frame (Signal); others let the protocol-level
    /// timeout elapse. Default no-op for transports without typing.
    async fn stop_typing(&self, channel: &str, recipient: &str) -> Result<(), ApiError> {
        let _ = (channel, recipient);
        Ok(())
    }
}

/// Result of [`TransportApi::fetch_attachment`]. Carries the raw
/// blob bytes plus the metadata the persistence layer needs to
/// build an `AttachmentRow` + emit the corresponding card.
#[derive(Debug, Clone)]
pub struct FetchedAttachment {
    /// Full attachment body. Already bounded by the consumer's
    /// per-attachment size cap.
    pub bytes: Vec<u8>,
    /// IANA media type the bridge reported (e.g. `image/jpeg`,
    /// `audio/aac`). Surfaces in the SPA's attachment chip + the
    /// `Content-Type` header on `/api/attachments/{id}`. Empty
    /// string acceptable when the bridge doesn't surface one;
    /// the persistence layer falls back to `application/octet-stream`.
    pub mime_type: String,
    /// Operator-visible filename. May come from the bridge's
    /// envelope or be synthesized from the attachment id when the
    /// bridge omitted it. Sanitized by the persistence layer
    /// before disk write.
    pub filename: Option<String>,
}

/// Compact summary of a transport-group surfaced by
/// [`TransportApi::list_groups`]. Mirrors the agent-facing tool
/// result shape so the host doesn't have to translate at the chat
/// surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportGroupSummary {
    /// Bridge's canonical group id — `foreign_id` for the binding
    /// store, opaque to callers above the transport tier.
    pub id: String,
    /// Operator-visible name. `None` for bridges that don't
    /// surface titled groups (chat platforms like Slack always do;
    /// signal-cli omits the title for "Note to Self"-style internal
    /// groups).
    pub name: Option<String>,
    /// Member count. Useful for the agent's "did the right people
    /// land in this group" check after a `create_group` /
    /// `add_group_members` call.
    pub member_count: u32,
}

/// Memory API. Reads cascade from the caller's trust class downward
/// (you can read at-or-below your level). Writes always land at your
/// own trust class — there's no escalation.
#[async_trait]
pub trait MemoryApi: Send + Sync {
    /// Read a value, scanning trust classes the caller is allowed to
    /// see. Returns `Ok(None)` if no entry is visible.
    async fn read(&self, scope: &str, key: &str) -> Result<Option<String>, ApiError>;
    /// Write a value at the caller's own trust class.
    async fn write(&self, scope: &str, key: &str, value: &str) -> Result<(), ApiError>;
    /// List visible keys in a scope, optionally prefix-filtered.
    async fn list(&self, scope: &str, prefix: &str) -> Result<Vec<MemoryListEntry>, ApiError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_parse_roundtrips_every_variant() {
        for c in [
            Capability::ConversationRead,
            Capability::ConversationWrite,
            Capability::MemoryRead,
            Capability::MemoryWrite,
            Capability::TaskRead,
            Capability::TaskWrite,
            Capability::ScheduleRead,
            Capability::ScheduleWrite,
            Capability::WebFetch,
            Capability::Search,
            Capability::Notify,
            Capability::SubagentSpawn,
            Capability::ResearchSpawn,
            Capability::ResearchRead,
            Capability::AttachmentSend,
            Capability::Transport,
        ] {
            assert_eq!(Capability::parse(c.as_str()), Some(c));
        }
        assert_eq!(Capability::parse("not_a_real_capability"), None);
    }

    #[test]
    fn tool_ctx_empty_leaves_transport_slot_unpopulated() {
        // Belt-and-suspenders: the `Transport` capability is opt-in
        // per tool. A tool that didn't declare it must NOT find a
        // transport handle in its ctx — that's the whole point of
        // capability-scoped contexts.
        let ctx = ToolCtx::empty(
            ConversationId::from("c"),
            "Controller",
            Arc::new(SystemClock),
        );
        assert!(ctx.transport.is_none());
    }

    #[test]
    fn tool_latency_parse_roundtrips() {
        for l in [ToolLatency::Low, ToolLatency::Medium, ToolLatency::High] {
            assert_eq!(ToolLatency::parse(l.as_str()), Some(l));
        }
        assert_eq!(ToolLatency::parse("instant"), None);
    }

    #[test]
    fn tool_source_str_and_parse_match_constants() {
        assert_eq!(ToolSource::Builtin.as_str(), "builtin");
        assert_eq!(ToolSource::Plugin.as_str(), "plugin");
        assert_eq!(ToolSource::Mcp.as_str(), "mcp");
    }

    #[test]
    fn tool_outcome_constructors() {
        match ToolOutcome::ok(serde_json::json!({"x": 1})) {
            ToolOutcome::Ok(v) => assert_eq!(v["x"], 1),
            other => panic!("unexpected: {other:?}"),
        }
        match ToolOutcome::err("bad", "no") {
            ToolOutcome::Err { code, message } => {
                assert_eq!(code, "bad");
                assert_eq!(message, "no");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match ToolOutcome::denied("nope") {
            ToolOutcome::Denied { reason } => assert_eq!(reason, "nope"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn api_error_maps_into_outcome_correctly() {
        match ApiError::NotAuthorized("trust too low".into()).into_outcome() {
            ToolOutcome::Denied { reason } => assert!(reason.contains("trust")),
            other => panic!("expected Denied, got {other:?}"),
        }
        match ApiError::NotFound("event 42".into()).into_outcome() {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "not_found"),
            other => panic!("expected Err, got {other:?}"),
        }
        match ApiError::Validation("nope".into()).into_outcome() {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "invalid_argument"),
            other => panic!("expected Err, got {other:?}"),
        }
        match ApiError::Storage("io".into()).into_outcome() {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "storage_error"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn tool_ctx_empty_has_no_capabilities_populated() {
        let ctx = ToolCtx::empty(
            ConversationId::from("c1"),
            "Controller",
            Arc::new(SystemClock),
        );
        assert!(ctx.conversation.is_none());
        assert!(ctx.memory.is_none());
        assert_eq!(ctx.caller_trust, "Controller");
        assert_eq!(ctx.conversation_id.as_str(), "c1");
    }

    /// Stateless `ToolImpl`s satisfy the trait via simple structs.
    /// This regression guards against accidentally requiring `'a`
    /// lifetimes or other awkwardness in the trait shape.
    #[test]
    fn tool_impl_can_be_constructed_as_arc_dyn() {
        struct Echo {
            descriptor: ToolDescriptor,
        }

        #[async_trait]
        impl ToolImpl for Echo {
            fn descriptor(&self) -> &ToolDescriptor {
                &self.descriptor
            }
            async fn invoke(&self, _ctx: ToolCtx, args: Value) -> ToolOutcome {
                ToolOutcome::ok(args)
            }
        }

        let tool: Arc<dyn ToolImpl> = Arc::new(Echo {
            descriptor: ToolDescriptor {
                name: "echo".into(),
                description: "echoes args".into(),
                schema: serde_json::json!({"type": "object"}),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: vec!["Controller".into()],
            },
        });
        assert_eq!(tool.descriptor().name, "echo");
    }
}
