//! Automations data model + store (M2 of Automations).
//!
//! Owns `state_automations`. An automation is a typed graph:
//!
//!   * Exactly one [`TriggerDef`] (the bus-event kind + optional
//!     payload-match predicate).
//!   * N typed [`NodeDef`]s Ã¢â‚¬â€ Filter / Transform / Branch / Terminal
//!     in M2; AskAgent / CallPlugin / HttpFetch / Notify reserved.
//!   * Directed [`EdgeDef`]s connecting nodes (and the synthetic
//!     `"trigger"` and `"END"` sentinels).
//!
//! The whole definition serializes to JSON and rides in the
//! `definition` column. Mutations go through [`AutomationStore::upsert`]
//! which runs the validator pre-write (so a malformed graph never
//! lands on disk). The runtime in `execlaw-server` reads back via
//! [`AutomationStore::list_for_kind`] (matcher hot path) and
//! [`AutomationStore::get`] (full-fetch on dispatch).

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

/// Reserved node-id sentinels. The runtime treats them specially.
pub const TRIGGER_SENTINEL: &str = "trigger";
pub const END_SENTINEL: &str = "END";

/// The kinds we know how to execute as of M2. Reserved kinds are
/// stored in the JSON but rejected by the validator with a
/// not-yet-implemented error Ã¢â‚¬â€ so a future schema bump can land
/// without breaking persisted definitions that pre-declared them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum NodeKind {
    /// Drop the run if the Rhai expression evaluates falsy.
    Filter,
    /// Run the Rhai expression for its value; the result becomes
    /// the node's `output`, available to downstream `{{<node_id>.*}}`
    /// templates.
    Transform,
    /// Multi-way switch: each `case` is a (Rhai-expr, edge-target).
    /// The first matching case picks the outgoing edge. If none
    /// match the node fails Ã¢â‚¬â€ author should provide a default
    /// (`when: "true"`) case.
    Branch,
    /// Explicit no-op end. Optional Ã¢â‚¬â€ a node with no outgoing edges
    /// also ends the run.
    Terminal,
    /// Pre-turn middleware mutator (Phase A of the Flows redesign).
    /// Runs a Rhai value expression that must evaluate to a string;
    /// the result becomes the rewritten user prompt the chat handler
    /// hands to the turn driver. Output shape: `{ "text": "<string>" }`
    /// in `state[node_id]` so the harvester in `flow_middleware`
    /// picks it up.
    RewritePrompt,
    /// Pre-turn middleware mutator (Phase B). Appends skill names to
    /// the upcoming chat turn's `applied_skill_names` list — the
    /// composer's skill-prepend resolver folds them into the
    /// user-message body. Output shape: `{ "skills": ["name", ...] }`.
    SetSkills,
    /// Pre-turn middleware mutator (Phase B). Appends tool names to
    /// the upcoming chat turn's `caller_caps`. Additive only — to
    /// narrow the tool palette, use SetTrust to downgrade to a more
    /// restrictive trust class. Output shape: `{ "tools": ["name", ...] }`.
    SetTools,
    /// Pre-turn middleware mutator (Phase B). Overrides the chat
    /// turn's `caller_trust`. Scalar last-writer-wins across flows.
    /// Output shape: `{ "trust": "controller" | "known_high" | ... }`.
    SetTrust,
    /// Pre-turn middleware mutator (Phase B). Appends attachment IDs
    /// to the upcoming chat turn's persisted attachments list.
    /// Validator checks shape only; existence is resolved at the
    /// chat handler's attachment-decode step. Output shape:
    /// `{ "attachment_ids": ["id", ...] }`.
    AddAttachment,
    /// Pre-turn middleware mutator (Phase B). Prepends an inline
    /// memory block to the upcoming chat turn's user message,
    /// wrapped in `<memory>…</memory>` (same pattern as skill
    /// prepends). Multiple AddMemory nodes accumulate. Output
    /// shape: `{ "text": "<string>" }`.
    AddMemory,
    // Reserved (validator rejects with NotYetImplemented):
    AskAgent,
    CallPlugin,
    AppendToChat,
    HttpFetch,
    Notify,
    AwaitApproval,
    CallAutomation,
    Parallel,
    Join,
    /// M6 â€” emit a reply through the ReplyRouter using
    /// `envelope.origin`. Validator rejects this kind in flows whose
    /// trigger has `expects_reply = false`.
    SendReply,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Filter => "Filter",
            NodeKind::Transform => "Transform",
            NodeKind::Branch => "Branch",
            NodeKind::Terminal => "Terminal",
            NodeKind::RewritePrompt => "RewritePrompt",
            NodeKind::SetSkills => "SetSkills",
            NodeKind::SetTools => "SetTools",
            NodeKind::SetTrust => "SetTrust",
            NodeKind::AddAttachment => "AddAttachment",
            NodeKind::AddMemory => "AddMemory",
            NodeKind::AskAgent => "AskAgent",
            NodeKind::CallPlugin => "CallPlugin",
            NodeKind::AppendToChat => "AppendToChat",
            NodeKind::HttpFetch => "HttpFetch",
            NodeKind::Notify => "Notify",
            NodeKind::AwaitApproval => "AwaitApproval",
            NodeKind::CallAutomation => "CallAutomation",
            NodeKind::Parallel => "Parallel",
            NodeKind::Join => "Join",
            NodeKind::SendReply => "SendReply",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Filter" => Some(NodeKind::Filter),
            "Transform" => Some(NodeKind::Transform),
            "Branch" => Some(NodeKind::Branch),
            "Terminal" => Some(NodeKind::Terminal),
            "RewritePrompt" => Some(NodeKind::RewritePrompt),
            "SetSkills" => Some(NodeKind::SetSkills),
            "SetTools" => Some(NodeKind::SetTools),
            "SetTrust" => Some(NodeKind::SetTrust),
            "AddAttachment" => Some(NodeKind::AddAttachment),
            "AddMemory" => Some(NodeKind::AddMemory),
            "AskAgent" => Some(NodeKind::AskAgent),
            "CallPlugin" => Some(NodeKind::CallPlugin),
            "AppendToChat" => Some(NodeKind::AppendToChat),
            "HttpFetch" => Some(NodeKind::HttpFetch),
            "Notify" => Some(NodeKind::Notify),
            "AwaitApproval" => Some(NodeKind::AwaitApproval),
            "CallAutomation" => Some(NodeKind::CallAutomation),
            "Parallel" => Some(NodeKind::Parallel),
            "Join" => Some(NodeKind::Join),
            "SendReply" => Some(NodeKind::SendReply),
            _ => None,
        }
    }

    /// `true` for kinds the M2 executor knows how to run. Reserved
    /// kinds (AskAgent etc.) round-trip through persistence but
    /// fail at validate-time with `NotYetImplemented`.
    pub fn is_implemented(&self) -> bool {
        matches!(
            self,
            NodeKind::Filter
                | NodeKind::Transform
                | NodeKind::Branch
                | NodeKind::Terminal
                | NodeKind::RewritePrompt
                | NodeKind::SetSkills
                | NodeKind::SetTools
                | NodeKind::SetTrust
                | NodeKind::AddAttachment
                | NodeKind::AddMemory
                | NodeKind::AskAgent
                | NodeKind::Notify
                | NodeKind::CallPlugin
                | NodeKind::SendReply
        )
    }
}

/// One exit tool the agent must choose between to terminate the
/// `AskAgent` turn. The tool's `args_schema` is OpenAI-style JSON
/// Schema, surfaced verbatim to the model so it knows what arguments
/// to fill in. The agent's chosen tool name becomes the node's edge-
/// routing key; the args become the node's output (available to
/// downstream `{{ask_agent.args.*}}` references).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ExitToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments. Pass through to the
    /// `tools[].function.parameters` field of the chat request.
    #[schema(value_type = Object)]
    pub args_schema: serde_json::Value,
}

/// Strongly-typed AskAgent node config. Persisted as `node.config`
/// JSON; parsed by the runtime at execute time.
///
/// Locked decisions from v3.2:
///   * Per-flow `reasoning_tools` palette (subset of `KnownLimited.allowed_tools`).
///   * `max_turns` defaults to 3 (single-turn enforced in M3a; multi-turn loop in a follow-up).
///   * First exit-tool call wins; later calls in the same turn are logged and ignored.
///   * Missing exit-tool call by `max_turns` Ã¢â€ â€™ node fails.
///   * Vision-required attachments on a text-only model Ã¢â€ â€™ node fails fast.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct AskAgentConfig {
    /// The user-message body sent to the agent. Rendered as-is (no
    /// templating in M3 Ã¢â‚¬â€ author writes the literal prompt or uses
    /// upstream Transform nodes to compose it).
    pub prompt: String,
    /// Attachment refs. Today each entry is either a `data:` URL or
    /// an `https://` URL the model can fetch. Empty for text-only
    /// turns; non-empty turns are vision-required and probe the
    /// model's capability before sending.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Reasoning-time tool palette. Must be a subset of the
    /// `KnownLimited` trust profile's `allowed_tools` (enforced at
    /// invoke time). Empty in M3a since multi-turn-with-tool-calls
    /// is a follow-up; reserved so author intent persists.
    #[serde(default)]
    pub reasoning_tools: Vec<String>,
    /// Synthesized terminal tools the agent must choose between.
    /// Exactly one will be picked; its name routes the outgoing
    /// edge and its args become the node's output.
    pub exit_tools: Vec<ExitToolDef>,
    /// Per-node `max_turns` override. `None` means "use the default
    /// (3)". M3a treats anything Ã¢â€°Â¥ 1 as a single-turn call.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

impl AskAgentConfig {
    pub fn effective_max_turns(&self) -> u32 {
        self.max_turns.unwrap_or(3).max(1)
    }
}

/// Parse an AskAgent node's `config` JSON. Returns `Err(_)` with a
/// descriptive message that the validator + runtime surface verbatim.
pub fn parse_ask_agent_config(config: &serde_json::Value) -> Result<AskAgentConfig, String> {
    serde_json::from_value::<AskAgentConfig>(config.clone())
        .map_err(|e| format!("AskAgent config: {e}"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct TriggerDef {
    /// The bus-event kind to subscribe to. Free-form string (M6+);
    /// well-known kinds have type-safe constants on
    /// [`BusEventKind`] (`BusEventKind::WebhookReceived.as_str()`),
    /// but plugin-defined kinds like `whatsapp.message.received`
    /// flow through as their literal string identity.
    pub kind: String,
    /// Optional payload-match predicate, as a Rhai expression
    /// evaluated against `{ event: { id, kind, source, payload,
    /// envelope } }`. `None` means "match every event of this kind".
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct NodeDef {
    pub id: String,
    pub kind: NodeKind,
    /// Kind-specific config. Schema:
    ///   * Filter:    `{ "expr": "<rhai bool>" }`
    ///   * Transform: `{ "expr": "<rhai value>" }`
    ///   * Branch:    `{ "cases": [{ "when": "<rhai bool>", "edge": "<node_id>" }, ...] }`
    ///   * Terminal:  `{}`
    ///   * AskAgent:  see `AskAgentConfig`
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: serde_json::Value,
    /// Optional canvas-coordinate hint for the SPA's ReactFlow
    /// editor. `None` means "the editor computes a default layout";
    /// `Some({x, y})` is the operator's manual placement, persisted so
    /// reloads land on the same canvas layout. The runtime ignores
    /// this field Ã¢â‚¬â€ it's purely UI metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<NodePosition>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct NodePosition {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct EdgeDef {
    /// Source: a node id, or `TRIGGER_SENTINEL` for the entry edge.
    pub from: String,
    /// Destination: a node id, or `END_SENTINEL` for terminal.
    pub to: String,
    /// Optional Rhai bool predicate. `None` = unconditional edge.
    /// Evaluated against the same scope as Filter/Transform plus
    /// the source node's output bound as `<from>`.
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct AutomationDef {
    pub trigger: TriggerDef,
    pub nodes: Vec<NodeDef>,
    pub edges: Vec<EdgeDef>,
}

/// Stored row.
#[derive(Debug, Clone)]
pub struct AutomationRow {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub definition: AutomationDef,
    pub created_at: i64,
    pub updated_at: i64,
    /// M6 â€” provenance. `"operator"` for operator-authored,
    /// `"core"` for built-in defaults, `"plugin:<id>"` for plugin-
    /// shipped defaults. Deletion is gated to operator-source rows
    /// only; defaults are managed by the install lifecycle.
    pub source: String,
    /// M6 â€” `true` when the operator has edited a non-operator
    /// row. Plugin upgrades surface a diff card instead of silently
    /// overwriting these. Default rows that the operator hasn't
    /// touched stay `false`.
    pub operator_modified: bool,
}

impl AutomationRow {
    /// `true` for rows the operator did NOT author â€” built-in core
    /// defaults + plugin-shipped defaults. The delete endpoint
    /// refuses these (they're managed by the install lifecycle â€”
    /// uninstall the plugin, or the row is removed by a future
    /// migration if core retires it). The SPA hides the delete
    /// button for these rows.
    pub fn is_default(&self) -> bool {
        self.source != "operator"
    }
}

#[derive(Debug, Clone)]
pub struct AutomationUpsert {
    /// `None` on create, `Some` on update.
    pub id: Option<String>,
    pub name: String,
    pub enabled: bool,
    pub definition: AutomationDef,
}

/// Result of [`AutomationStore::delete`]. Carries enough info for
/// the HTTP layer to choose the right status code + message.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    /// Refused because the row is a default flow shipped by core or
    /// a plugin. Carries the `source` value so the error message
    /// can say which install layer owns it.
    RefusedDefault {
        source: String,
    },
}

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("definition encode: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("validation: {0}")]
    Validation(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// Validate an automation definition against the M2 executor's
/// expectations. Run at upsert time so malformed defs never persist;
/// also called by the validator endpoint for live editor feedback.
///
/// Rules:
///   * Every node `id` is non-empty and unique within the graph.
///   * Reserved sentinels (`trigger`, `END`) are not used as node
///     ids (they're edge endpoints only).
///   * Every `from`/`to` in edges either matches a node id or is
///     a sentinel.
///   * Every node has at least one outgoing edge OR is `Terminal`.
///     (A non-Terminal node with no outgoing edges runs to no-op
///     end; we reject as ambiguous Ã¢â‚¬â€ the author probably forgot
///     an edge.)
///   * All node kinds are implemented (rejects AskAgent / Notify /
///     etc. with NotYetImplemented).
///
/// Rhai expression *syntax* is NOT validated here Ã¢â‚¬â€ that requires
/// constructing an engine, which lives in the server crate. The
/// server-side validator at the API/runtime layer rejects malformed
/// Rhai. Persisting a parseable JSON with bad Rhai is harmless: the
/// run fails the first time it's exercised.
pub fn validate(def: &AutomationDef) -> Result<(), AutomationError> {
    use std::collections::HashSet;
    let mut ids = HashSet::with_capacity(def.nodes.len());
    for n in &def.nodes {
        if n.id.is_empty() {
            return Err(AutomationError::Validation("empty node id".into()));
        }
        if n.id == TRIGGER_SENTINEL || n.id == END_SENTINEL {
            return Err(AutomationError::Validation(format!(
                "node id '{}' is reserved",
                n.id
            )));
        }
        if !ids.insert(n.id.clone()) {
            return Err(AutomationError::Validation(format!(
                "duplicate node id '{}'",
                n.id
            )));
        }
        if !n.kind.is_implemented() {
            return Err(AutomationError::Validation(format!(
                "node kind '{}' not yet implemented (reserved in v3.2 design, lands in a later milestone)",
                n.kind.as_str()
            )));
        }
        // Kind-specific shape checks. We re-parse the config into the
        // typed struct so a missing field surfaces at save time, not
        // at first-run.
        if matches!(n.kind, NodeKind::AskAgent) {
            let cfg = parse_ask_agent_config(&n.config).map_err(AutomationError::Validation)?;
            if cfg.prompt.trim().is_empty() {
                return Err(AutomationError::Validation(format!(
                    "AskAgent node '{}': prompt is empty",
                    n.id
                )));
            }
            if cfg.exit_tools.is_empty() {
                return Err(AutomationError::Validation(format!(
                    "AskAgent node '{}': exit_tools is empty (the agent must have at least one tool to call)",
                    n.id
                )));
            }
            let mut tool_names = std::collections::HashSet::new();
            for t in &cfg.exit_tools {
                if t.name.trim().is_empty() {
                    return Err(AutomationError::Validation(format!(
                        "AskAgent node '{}': exit tool with empty name",
                        n.id
                    )));
                }
                if !tool_names.insert(t.name.clone()) {
                    return Err(AutomationError::Validation(format!(
                        "AskAgent node '{}': duplicate exit tool name '{}'",
                        n.id, t.name
                    )));
                }
            }
            // Reasoning_tools is reserved in M3a Ã¢â‚¬â€ author can declare
            // intent but we don't surface those tools to the model yet.
            // Document this implicitly: empty list is fine, non-empty
            // is also fine (forward-compatible).
            let _ = cfg.reasoning_tools;
            let _ = cfg.effective_max_turns();
        }
        if matches!(n.kind, NodeKind::Notify) {
            // title is required + non-empty; severity (if present)
            // must parse to a known variant. detail + source are free.
            let title = n.config.get("title").and_then(|v| v.as_str()).unwrap_or("");
            if title.trim().is_empty() {
                return Err(AutomationError::Validation(format!(
                    "Notify node '{}': config.title is required and must be a non-empty string",
                    n.id
                )));
            }
            if let Some(sev) = n.config.get("severity").and_then(|v| v.as_str())
                && crate::alerts::Severity::parse(sev).is_none()
            {
                return Err(AutomationError::Validation(format!(
                    "Notify node '{}': unknown severity '{}' (expected Critical|Error|Warning|Info)",
                    n.id, sev
                )));
            }
        }
        if matches!(n.kind, NodeKind::CallPlugin) {
            // tool is required + non-empty. args is optional (defaults
            // to empty object at runtime); when present it must be an
            // object so we can apply template rendering recursively.
            let tool = n.config.get("tool").and_then(|v| v.as_str()).unwrap_or("");
            if tool.trim().is_empty() {
                return Err(AutomationError::Validation(format!(
                    "CallPlugin node '{}': config.tool is required (registered tool name)",
                    n.id
                )));
            }
            if let Some(args) = n.config.get("args")
                && !args.is_object()
            {
                return Err(AutomationError::Validation(format!(
                    "CallPlugin node '{}': config.args must be a JSON object (got {})",
                    n.id,
                    args_kind(args),
                )));
            }
        }
        if matches!(n.kind, NodeKind::RewritePrompt) {
            // expr is required + non-empty Rhai source. Cheap shape
            // check at save time; the Rhai engine runs the real
            // parse during execute (eval failures land in the trace).
            let expr = n.config.get("expr").and_then(|v| v.as_str()).unwrap_or("");
            if expr.trim().is_empty() {
                return Err(AutomationError::Validation(format!(
                    "RewritePrompt node '{}': config.expr is required and must be a non-empty Rhai value expression returning a string",
                    n.id
                )));
            }
        }
        if matches!(n.kind, NodeKind::SetSkills) {
            // skills: required non-empty array of non-empty strings.
            let arr = match n.config.get("skills").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => return Err(AutomationError::Validation(format!(
                    "SetSkills node '{}': config.skills is required and must be a JSON array of skill names",
                    n.id
                ))),
            };
            if arr.is_empty() {
                return Err(AutomationError::Validation(format!(
                    "SetSkills node '{}': config.skills must be non-empty (drop the node if you don't want to add skills)",
                    n.id
                )));
            }
            for s in arr {
                let name = s.as_str().unwrap_or("");
                if name.trim().is_empty() {
                    return Err(AutomationError::Validation(format!(
                        "SetSkills node '{}': skill names must be non-empty strings",
                        n.id
                    )));
                }
            }
        }
        if matches!(n.kind, NodeKind::SetTools) {
            // tools: required non-empty array of non-empty strings.
            let arr = match n.config.get("tools").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => return Err(AutomationError::Validation(format!(
                    "SetTools node '{}': config.tools is required and must be a JSON array of tool names",
                    n.id
                ))),
            };
            if arr.is_empty() {
                return Err(AutomationError::Validation(format!(
                    "SetTools node '{}': config.tools must be non-empty",
                    n.id
                )));
            }
            for s in arr {
                let name = s.as_str().unwrap_or("");
                if name.trim().is_empty() {
                    return Err(AutomationError::Validation(format!(
                        "SetTools node '{}': tool names must be non-empty strings",
                        n.id
                    )));
                }
            }
        }
        if matches!(n.kind, NodeKind::SetTrust) {
            // trust: required string matching a known TrustClass.
            let trust = n.config.get("trust").and_then(|v| v.as_str()).unwrap_or("");
            if trust.trim().is_empty() {
                return Err(AutomationError::Validation(format!(
                    "SetTrust node '{}': config.trust is required",
                    n.id
                )));
            }
            if crate::event_envelope::TrustClass::parse(trust).is_none() {
                return Err(AutomationError::Validation(format!(
                    "SetTrust node '{}': unknown trust class '{}' (expected controller | known_high | known_limited | cold_contact | blocked)",
                    n.id, trust
                )));
            }
        }
        if matches!(n.kind, NodeKind::AddAttachment) {
            // attachment_ids: required non-empty array of non-empty
            // strings. Existence check defers to the chat handler's
            // attachment-decode step (IDs may be templated via Rhai
            // in the future; existence checks at save-time would be
            // brittle anyway).
            let arr = match n.config.get("attachment_ids").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => return Err(AutomationError::Validation(format!(
                    "AddAttachment node '{}': config.attachment_ids is required and must be a JSON array",
                    n.id
                ))),
            };
            if arr.is_empty() {
                return Err(AutomationError::Validation(format!(
                    "AddAttachment node '{}': config.attachment_ids must be non-empty",
                    n.id
                )));
            }
            for s in arr {
                let id = s.as_str().unwrap_or("");
                if id.trim().is_empty() {
                    return Err(AutomationError::Validation(format!(
                        "AddAttachment node '{}': attachment ids must be non-empty strings",
                        n.id
                    )));
                }
            }
        }
        if matches!(n.kind, NodeKind::AddMemory) {
            // text: required non-empty string. May contain
            // `{{event.payload.x}}` template tokens — rendered at
            // execute time, not validated here.
            let text = n.config.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.trim().is_empty() {
                return Err(AutomationError::Validation(format!(
                    "AddMemory node '{}': config.text is required and must be a non-empty string",
                    n.id
                )));
            }
        }
        if matches!(n.kind, NodeKind::SendReply) {
            // source must be one of the three known values (or
            // absent â€” defaults to "from_agent").
            let source = n
                .config
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("from_agent");
            if !matches!(source, "from_agent" | "from_node_output" | "template") {
                return Err(AutomationError::Validation(format!(
                    "SendReply node '{}': unknown source '{}' (expected from_agent | from_node_output | template)",
                    n.id, source
                )));
            }
            // For template source, `text` must be present.
            if source == "template"
                && n.config.get("text").and_then(|v| v.as_str()).is_none()
            {
                return Err(AutomationError::Validation(format!(
                    "SendReply node '{}': template source requires config.text",
                    n.id
                )));
            }
            // target_override (if present) must be a valid OriginRef.
            if let Some(t) = n.config.get("target_override")
                && !t.is_null()
                && serde_json::from_value::<crate::event_envelope::OriginRef>(t.clone()).is_err()
            {
                return Err(AutomationError::Validation(format!(
                    "SendReply node '{}': target_override is not a valid OriginRef shape",
                    n.id
                )));
            }
        }
    }
    for e in &def.edges {
        if e.from != TRIGGER_SENTINEL && !ids.contains(&e.from) {
            return Err(AutomationError::Validation(format!(
                "edge.from references unknown node '{}'",
                e.from
            )));
        }
        if e.to != END_SENTINEL && !ids.contains(&e.to) {
            return Err(AutomationError::Validation(format!(
                "edge.to references unknown node '{}'",
                e.to
            )));
        }
    }
    // Every non-Terminal node must have Ã¢â€°Â¥1 outgoing edge OR be the
    // explicit end (i.e., have Terminal kind).
    for n in &def.nodes {
        if matches!(n.kind, NodeKind::Terminal) {
            continue;
        }
        let has_outgoing = def.edges.iter().any(|e| e.from == n.id);
        if !has_outgoing {
            return Err(AutomationError::Validation(format!(
                "node '{}' has no outgoing edge and is not Terminal (orphaned)",
                n.id
            )));
        }
    }
    // Exactly one entry edge from the trigger sentinel Ã¢â‚¬â€ otherwise
    // the run has no starting point.
    let entry_edges = def
        .edges
        .iter()
        .filter(|e| e.from == TRIGGER_SENTINEL)
        .count();
    if entry_edges == 0 {
        return Err(AutomationError::Validation(
            "no edge from trigger Ã¢â‚¬â€ automation has no entry point".into(),
        ));
    }
    Ok(())
}

/// Lower-cased JSON-type label for validator error messages, so the
/// operator sees "got string" rather than `String("â€¦")`.
fn args_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub struct AutomationStore<'a> {
    db: &'a Database,
}

impl<'a> AutomationStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Insert or update. Mints a fresh ULID on create; passes through
    /// the operator-supplied id on update.
    pub fn upsert(
        &self,
        req: &AutomationUpsert,
        now: i64,
    ) -> Result<AutomationRow, AutomationError> {
        validate(&req.definition)?;
        let id = req.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
        let def_json = serde_json::to_string(&req.definition)?;
        let enabled_flag: i64 = if req.enabled { 1 } else { 0 };
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_automations \
                 (id, name, enabled, definition, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                   name = excluded.name, \
                   enabled = excluded.enabled, \
                   definition = excluded.definition, \
                   updated_at = excluded.updated_at",
                params![&id, &req.name, enabled_flag, &def_json, now],
            )?;
            Ok(())
        })?;
        self.get(&id)?.ok_or_else(|| AutomationError::NotFound(id))
    }

    pub fn get(&self, id: &str) -> Result<Option<AutomationRow>, AutomationError> {
        let row = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, enabled, definition, created_at, updated_at, \
                        COALESCE(source, 'operator'), COALESCE(operator_modified, 0) \
                 FROM state_automations WHERE id = ?1",
            )?;
            let r = stmt
                .query_row([id], |r| {
                    let def_str: String = r.get(3)?;
                    let definition: AutomationDef =
                        serde_json::from_str(&def_str).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    let enabled_flag: i64 = r.get(2)?;
                    Ok(AutomationRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        enabled: enabled_flag != 0,
                        definition,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                        source: r.get(6)?,
                        operator_modified: r.get::<_, i64>(7)? != 0,
                    })
                })
                .ok();
            Ok(r)
        })?;
        Ok(row)
    }

    pub fn list_all(&self) -> Result<Vec<AutomationRow>, AutomationError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, enabled, definition, created_at, updated_at, \
                        COALESCE(source, 'operator'), COALESCE(operator_modified, 0) \
                 FROM state_automations ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                let def_str: String = r.get(3)?;
                let definition: AutomationDef = serde_json::from_str(&def_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let enabled_flag: i64 = r.get(2)?;
                Ok(AutomationRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    enabled: enabled_flag != 0,
                    definition,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    source: r.get(6)?,
                    operator_modified: r.get::<_, i64>(7)? != 0,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    /// Matcher hot path Ã¢â‚¬â€ enabled automations whose trigger kind
    /// matches. The expression index `idx_automations_enabled_trigger_kind`
    /// keeps this cheap regardless of total automation count.
    pub fn list_enabled_for_kind(
        &self,
        kind: &str,
    ) -> Result<Vec<AutomationRow>, AutomationError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, enabled, definition, created_at, updated_at, \
                        COALESCE(source, 'operator'), COALESCE(operator_modified, 0) \
                 FROM state_automations \
                 WHERE enabled = 1 \
                   AND json_extract(definition, '$.trigger.kind') = ?1 \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([kind], |r| {
                let def_str: String = r.get(3)?;
                let definition: AutomationDef = serde_json::from_str(&def_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let enabled_flag: i64 = r.get(2)?;
                Ok(AutomationRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    enabled: enabled_flag != 0,
                    definition,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    source: r.get(6)?,
                    operator_modified: r.get::<_, i64>(7)? != 0,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    /// Delete an automation by id. Refuses to delete default flows
    /// (rows with `source != 'operator'`) â€” those are managed by the
    /// install lifecycle (plugin install / uninstall, core upgrades).
    /// Returns `DeleteOutcome::Refused` so the HTTP layer can surface
    /// a 403 with an actionable message.
    pub fn delete(&self, id: &str) -> Result<DeleteOutcome, AutomationError> {
        // Inspect first so we know whether to refuse + surface a
        // useful error to the operator instead of a silent no-op.
        let row = self.get(id)?;
        let Some(row) = row else {
            return Ok(DeleteOutcome::NotFound);
        };
        if row.is_default() {
            return Ok(DeleteOutcome::RefusedDefault {
                source: row.source.clone(),
            });
        }
        let n = self.db.with_conn(|c| {
            let n = c.execute("DELETE FROM state_automations WHERE id = ?1", params![id])?;
            Ok(n)
        })?;
        if n > 0 {
            Ok(DeleteOutcome::Deleted)
        } else {
            // Race: the row was deleted between our get() and the
            // execute(). Treat as NotFound rather than panicking.
            Ok(DeleteOutcome::NotFound)
        }
    }

    pub fn set_enabled(&self, id: &str, enabled: bool, now: i64) -> Result<bool, AutomationError> {
        let flag: i64 = if enabled { 1 } else { 0 };
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_automations SET enabled = ?2, updated_at = ?3 \
                 WHERE id = ?1",
                params![id, flag, now],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
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

    fn linear_def() -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "f1".into(),
                    kind: NodeKind::Filter,
                    config: serde_json::json!({"expr": "true"}),
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "f1".into(),
                    when: None,
                },
                EdgeDef {
                    from: "f1".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_minimal_linear_graph() {
        validate(&linear_def()).unwrap();
    }

    #[test]
    fn validate_rejects_duplicate_node_ids() {
        let mut def = linear_def();
        def.nodes.push(NodeDef {
            id: "f1".into(), // dup
            kind: NodeKind::Terminal,
            config: serde_json::json!({}),
            position: None,
        });
        let err = validate(&def).unwrap_err();
        assert!(matches!(err, AutomationError::Validation(_)));
        assert!(format!("{err}").contains("duplicate node id"));
    }

    #[test]
    fn validate_rejects_reserved_sentinels_as_ids() {
        let mut def = linear_def();
        def.nodes[0].id = TRIGGER_SENTINEL.into();
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("reserved"));
    }

    #[test]
    fn validate_rejects_edge_to_unknown_node() {
        let mut def = linear_def();
        def.edges.push(EdgeDef {
            from: "f1".into(),
            to: "nowhere".into(),
            when: None,
        });
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("edge.to references unknown node"));
    }

    #[test]
    fn validate_rejects_orphaned_node() {
        let mut def = linear_def();
        def.nodes.push(NodeDef {
            id: "orphan".into(),
            kind: NodeKind::Filter,
            config: serde_json::json!({"expr": "true"}),
            position: None,
        });
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("orphaned"));
    }

    #[test]
    fn validate_rejects_unimplemented_kinds() {
        let mut def = linear_def();
        // HttpFetch + Notify are still reserved in M3a.
        def.nodes[0].kind = NodeKind::HttpFetch;
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("not yet implemented"));
    }

    fn ask_agent_def() -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "ask".into(),
                    kind: NodeKind::AskAgent,
                    config: serde_json::json!({
                        "prompt": "Look at the image",
                        "attachments": [],
                        "reasoning_tools": [],
                        "exit_tools": [
                            {
                                "name": "notify",
                                "description": "Call on animal detection",
                                "args_schema": {
                                    "type": "object",
                                    "properties": {
                                        "species": {"type": "string"}
                                    }
                                }
                            },
                            {
                                "name": "ignore",
                                "description": "Call when no animal",
                                "args_schema": {"type": "object"}
                            }
                        ]
                    }),
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "ask".into(),
                    when: None,
                },
                EdgeDef {
                    from: "ask".into(),
                    to: "end".into(),
                    when: Some(r#"ask.tool == "notify""#.into()),
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_well_formed_ask_agent_node() {
        validate(&ask_agent_def()).unwrap();
    }

    #[test]
    fn validate_rejects_ask_agent_with_empty_prompt() {
        let mut def = ask_agent_def();
        let mut cfg = parse_ask_agent_config(&def.nodes[0].config).unwrap();
        cfg.prompt = "   ".into();
        def.nodes[0].config = serde_json::to_value(&cfg).unwrap();
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("prompt is empty"));
    }

    #[test]
    fn validate_rejects_ask_agent_with_empty_exit_tools() {
        let mut def = ask_agent_def();
        let mut cfg = parse_ask_agent_config(&def.nodes[0].config).unwrap();
        cfg.exit_tools.clear();
        def.nodes[0].config = serde_json::to_value(&cfg).unwrap();
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("exit_tools is empty"));
    }

    #[test]
    fn validate_rejects_ask_agent_with_duplicate_exit_tool_names() {
        let mut def = ask_agent_def();
        let mut cfg = parse_ask_agent_config(&def.nodes[0].config).unwrap();
        cfg.exit_tools.push(ExitToolDef {
            name: "notify".into(),
            description: "duplicate!".into(),
            args_schema: serde_json::json!({}),
        });
        def.nodes[0].config = serde_json::to_value(&cfg).unwrap();
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("duplicate exit tool name"));
    }

    #[test]
    fn validate_rejects_ask_agent_with_malformed_config() {
        let mut def = ask_agent_def();
        def.nodes[0].config = serde_json::json!({
            "prompt": "hi"
            // missing exit_tools Ã¢â‚¬â€ required field
        });
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("AskAgent config"));
    }

    /// Build a minimal `trigger -> notify -> terminal` def for the
    /// Notify validator tests (M6).
    fn notify_def(cfg: serde_json::Value) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "alert".into(),
                    kind: NodeKind::Notify,
                    config: cfg,
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "alert".into(),
                    when: None,
                },
                EdgeDef {
                    from: "alert".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_well_formed_notify_node() {
        let def = notify_def(serde_json::json!({
            "title": "Hello",
            "severity": "Info",
        }));
        validate(&def).unwrap();
    }

    #[test]
    fn validate_rejects_notify_with_missing_title() {
        let def = notify_def(serde_json::json!({}));
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("config.title"));
    }

    #[test]
    fn validate_rejects_notify_with_unknown_severity() {
        let def = notify_def(serde_json::json!({
            "title": "x",
            "severity": "Catastrophic",
        }));
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("severity"));
    }

    /// Build a minimal `trigger -> call_plugin -> terminal` def for
    /// the CallPlugin validator tests (M6).
    fn call_plugin_def(cfg: serde_json::Value) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "call".into(),
                    kind: NodeKind::CallPlugin,
                    config: cfg,
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "call".into(),
                    when: None,
                },
                EdgeDef {
                    from: "call".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_well_formed_call_plugin_node() {
        let def = call_plugin_def(serde_json::json!({
            "tool": "signal.send_message",
            "args": {"to": "+15551234", "body": "hi"},
        }));
        validate(&def).unwrap();
    }

    #[test]
    fn validate_rejects_call_plugin_with_missing_tool() {
        let def = call_plugin_def(serde_json::json!({"args": {}}));
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("config.tool"));
    }

    #[test]
    fn validate_rejects_call_plugin_with_non_object_args() {
        let def = call_plugin_def(serde_json::json!({
            "tool": "x",
            "args": "not-an-object",
        }));
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("args"));
    }

    // ---- RewritePrompt (Phase A of Flows middleware) ----

    fn rewrite_prompt_def(config: serde_json::Value) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "chat.prompt".to_owned(),
                when: None,
            },
            nodes: vec![NodeDef {
                id: "rw".into(),
                kind: NodeKind::RewritePrompt,
                config,
                position: None,
            }],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "rw".into(),
                    when: None,
                },
                EdgeDef {
                    from: "rw".into(),
                    to: END_SENTINEL.into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_well_formed_rewrite_prompt() {
        let def = rewrite_prompt_def(serde_json::json!({
            "expr": "\"[prefix] \" + event.payload.text"
        }));
        validate(&def).expect("well-formed RewritePrompt must validate");
    }

    #[test]
    fn validate_rejects_rewrite_prompt_with_missing_expr() {
        let def = rewrite_prompt_def(serde_json::json!({}));
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("config.expr"));
    }

    #[test]
    fn validate_rejects_rewrite_prompt_with_blank_expr() {
        let def = rewrite_prompt_def(serde_json::json!({"expr": "   "}));
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("non-empty"));
    }

    // ---- Phase B mutator validator tests ----

    fn single_mutator_def(kind: NodeKind, config: serde_json::Value) -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "chat.prompt".to_owned(),
                when: None,
            },
            nodes: vec![NodeDef {
                id: "m".into(),
                kind,
                config,
                position: None,
            }],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "m".into(),
                    when: None,
                },
                EdgeDef {
                    from: "m".into(),
                    to: END_SENTINEL.into(),
                    when: None,
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_well_formed_set_skills() {
        let def = single_mutator_def(
            NodeKind::SetSkills,
            serde_json::json!({"skills": ["notes_taker", "calendar"]}),
        );
        validate(&def).expect("well-formed SetSkills must validate");
    }

    #[test]
    fn validate_rejects_set_skills_missing_array() {
        let def = single_mutator_def(NodeKind::SetSkills, serde_json::json!({}));
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("config.skills"));
    }

    #[test]
    fn validate_rejects_set_skills_empty_array() {
        let def = single_mutator_def(NodeKind::SetSkills, serde_json::json!({"skills": []}));
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn validate_rejects_set_skills_blank_name() {
        let def = single_mutator_def(
            NodeKind::SetSkills,
            serde_json::json!({"skills": ["ok", "   "]}),
        );
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn validate_accepts_well_formed_set_tools() {
        let def = single_mutator_def(
            NodeKind::SetTools,
            serde_json::json!({"tools": ["python.execute"]}),
        );
        validate(&def).expect("well-formed SetTools must validate");
    }

    #[test]
    fn validate_rejects_set_tools_missing() {
        let def = single_mutator_def(NodeKind::SetTools, serde_json::json!({}));
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("config.tools"));
    }

    #[test]
    fn validate_accepts_set_trust_pascal_and_snake_case() {
        for trust in [
            "Controller", "controller",
            "KnownLimited", "known_limited",
            "Blocked", "blocked",
            "known_high", // SPA snake-case alias for KnownTrusted
        ] {
            let def = single_mutator_def(
                NodeKind::SetTrust,
                serde_json::json!({"trust": trust}),
            );
            validate(&def).unwrap_or_else(|e| panic!("trust '{trust}' should validate: {e}"));
        }
    }

    #[test]
    fn validate_rejects_set_trust_unknown_class() {
        let def = single_mutator_def(
            NodeKind::SetTrust,
            serde_json::json!({"trust": "super_admin"}),
        );
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("unknown trust class"));
    }

    #[test]
    fn validate_rejects_set_trust_missing() {
        let def = single_mutator_def(NodeKind::SetTrust, serde_json::json!({}));
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("config.trust"));
    }

    #[test]
    fn validate_accepts_well_formed_add_attachment() {
        let def = single_mutator_def(
            NodeKind::AddAttachment,
            serde_json::json!({"attachment_ids": ["att-123", "att-456"]}),
        );
        validate(&def).expect("well-formed AddAttachment must validate");
    }

    #[test]
    fn validate_rejects_add_attachment_empty_array() {
        let def = single_mutator_def(
            NodeKind::AddAttachment,
            serde_json::json!({"attachment_ids": []}),
        );
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn validate_accepts_well_formed_add_memory() {
        let def = single_mutator_def(
            NodeKind::AddMemory,
            serde_json::json!({"text": "Operator prefers concise replies."}),
        );
        validate(&def).expect("well-formed AddMemory must validate");
    }

    #[test]
    fn validate_rejects_add_memory_blank_text() {
        let def = single_mutator_def(NodeKind::AddMemory, serde_json::json!({"text": "   "}));
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn validate_rejects_add_memory_missing_text() {
        let def = single_mutator_def(NodeKind::AddMemory, serde_json::json!({}));
        let err = format!("{}", validate(&def).unwrap_err());
        assert!(err.contains("config.text"));
    }

    #[test]
    fn ask_agent_config_default_max_turns_is_three() {
        let cfg = AskAgentConfig {
            prompt: "x".into(),
            attachments: vec![],
            reasoning_tools: vec![],
            exit_tools: vec![ExitToolDef {
                name: "ok".into(),
                description: "".into(),
                args_schema: serde_json::json!({}),
            }],
            max_turns: None,
        };
        assert_eq!(cfg.effective_max_turns(), 3);
    }

    #[test]
    fn validate_rejects_no_trigger_edge() {
        let mut def = linear_def();
        def.edges.retain(|e| e.from != TRIGGER_SENTINEL);
        let err = validate(&def).unwrap_err();
        assert!(format!("{err}").contains("no edge from trigger"));
    }

    #[test]
    fn upsert_round_trips_definition() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "ring-watch".into(),
                    enabled: true,
                    definition: linear_def(),
                },
                1000,
            )
            .unwrap();
        let fetched = store.get(&row.id).unwrap().unwrap();
        assert_eq!(fetched.name, "ring-watch");
        assert!(fetched.enabled);
        assert_eq!(fetched.definition, linear_def());
        assert_eq!(fetched.created_at, 1000);
        assert_eq!(fetched.updated_at, 1000);
    }

    #[test]
    fn upsert_rejects_invalid_definition() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let mut bad = linear_def();
        bad.edges.clear(); // no trigger edge
        let err = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "broken".into(),
                    enabled: true,
                    definition: bad,
                },
                1000,
            )
            .unwrap_err();
        assert!(matches!(err, AutomationError::Validation(_)));
        // And the row was never written.
        assert_eq!(store.list_all().unwrap().len(), 0);
    }

    #[test]
    fn list_enabled_for_kind_filters_correctly() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        // Two automations on WebhookReceived, one on RoutineFired,
        // one disabled.
        for (name, kind, enabled) in [
            ("wh-1", "webhook.received", true),
            ("wh-2", "webhook.received", true),
            ("rt-1", "routine.fired", true),
            ("wh-disabled", "webhook.received", false),
        ] {
            let mut def = linear_def();
            def.trigger.kind = kind.to_owned();
            store
                .upsert(
                    &AutomationUpsert {
                        id: None,
                        name: name.into(),
                        enabled,
                        definition: def,
                    },
                    1000,
                )
                .unwrap();
        }
        let webhook = store
            .list_enabled_for_kind("webhook.received")
            .unwrap();
        assert_eq!(webhook.len(), 2);
        let routine = store
            .list_enabled_for_kind("routine.fired")
            .unwrap();
        assert_eq!(routine.len(), 1);
        let socket = store
            .list_enabled_for_kind("socket.message")
            .unwrap();
        assert_eq!(socket.len(), 0);
    }

    #[test]
    fn set_enabled_toggles_flag() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "toggle".into(),
                    enabled: true,
                    definition: linear_def(),
                },
                1000,
            )
            .unwrap();
        assert!(store.set_enabled(&row.id, false, 2000).unwrap());
        assert!(!store.get(&row.id).unwrap().unwrap().enabled);
        assert!(store.set_enabled(&row.id, true, 3000).unwrap());
        assert!(store.get(&row.id).unwrap().unwrap().enabled);
    }

    #[test]
    fn delete_removes_row() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "ephemeral".into(),
                    enabled: true,
                    definition: linear_def(),
                },
                1000,
            )
            .unwrap();
        assert_eq!(store.delete(&row.id).unwrap(), DeleteOutcome::Deleted);
        assert!(store.get(&row.id).unwrap().is_none());
        // Idempotent â€” second delete reports NotFound, not an error.
        assert_eq!(store.delete(&row.id).unwrap(), DeleteOutcome::NotFound);
    }

    #[test]
    fn delete_refuses_default_source_rows() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "default web prompt flow".into(),
                    enabled: true,
                    definition: linear_def(),
                },
                1000,
            )
            .unwrap();
        // Promote to a core-shipped default row by patching source
        // directly (operator manifests would normally do this via
        // the plugin-host install path).
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_automations SET source = 'core' WHERE id = ?1",
                rusqlite::params![row.id],
            )?;
            Ok(())
        })
        .unwrap();
        let outcome = store.delete(&row.id).unwrap();
        match outcome {
            DeleteOutcome::RefusedDefault { source } => assert_eq!(source, "core"),
            other => panic!("expected RefusedDefault, got {other:?}"),
        }
        // Row still exists.
        assert!(store.get(&row.id).unwrap().is_some());
    }

    #[test]
    fn delete_refuses_plugin_source_rows() {
        let db = fresh_db();
        let store = AutomationStore::new(&db);
        let row = store
            .upsert(
                &AutomationUpsert {
                    id: None,
                    name: "whatsapp default".into(),
                    enabled: true,
                    definition: linear_def(),
                },
                1000,
            )
            .unwrap();
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_automations SET source = 'plugin:whatsapp' WHERE id = ?1",
                rusqlite::params![row.id],
            )?;
            Ok(())
        })
        .unwrap();
        let outcome = store.delete(&row.id).unwrap();
        assert!(
            matches!(outcome, DeleteOutcome::RefusedDefault { source } if source == "plugin:whatsapp")
        );
    }
}
