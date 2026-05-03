//! Model-facing tool surface for the skill subsystem.
//!
//! Two families:
//!
//! - **Read tools** (always offered to the agent):
//!     - `skills.list`     — compact name+description+state+version list
//!     - `skills.view`     — full body of one skill (activation event)
//!     - `skills.resource` — bundled resource bytes
//!     - `skills.search`   — FTS5 query (Phase D rolls this behind a flag)
//!
//! - **Write tools** (admin-only):
//!     - `skills.create`   — author a new skill in `trial` state
//!     - `skills.update`   — add a new version to an existing skill
//!     - `skills.promote`  — `trial` → `stable`
//!     - `skills.archive`  — retire a skill (invisible to the agent)
//!
//! Admin gating is **defense in depth**: the access gate is set at the
//! `default_allowed_classes` layer (only `Controller` by default), but
//! the tool body also rechecks `ctx.caller_trust == "Controller"` so an
//! operator-edited `config_tool_access` row that mistakenly grants the
//! tool to a lower class still gets rebuffed.

use crate::model::{
    NewSkill, NewSkillVersion, RegistrationKind, ResourceBlob, SkillError, validate_skill_name,
};
use crate::scanner::Strictness;
use crate::store::SkillStore;
use async_trait::async_trait;
use execlaw_core::tool::{
    ToolCtx, ToolDescriptor, ToolImpl, ToolLatency, ToolOutcome, ToolSource,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const ADMIN_CLASS: &str = "Controller";

fn classes_for_read() -> Vec<String> {
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
        "UnknownPending".into(),
    ]
}

fn classes_for_admin_write() -> Vec<String> {
    vec!["Controller".into()]
}

fn require_admin(ctx: &ToolCtx) -> Result<(), ToolOutcome> {
    if ctx.caller_trust == ADMIN_CLASS {
        Ok(())
    } else {
        Err(ToolOutcome::denied(format!(
            "skills.* write tools require Controller trust class; caller is {}",
            ctx.caller_trust
        )))
    }
}

fn skill_error_to_outcome(e: SkillError) -> ToolOutcome {
    match e {
        SkillError::InvalidName(name) => ToolOutcome::err(
            "invalid_argument",
            format!("invalid skill name {name:?}; must match `<namespace>/<name>`"),
        ),
        SkillError::NotFound(n) => ToolOutcome::err("not_found", n),
        SkillError::AlreadyExists(n) => ToolOutcome::err("already_exists", n),
        SkillError::InvalidStateTransition { from, to } => ToolOutcome::err(
            "invalid_state_transition",
            format!("cannot transition from {from} to {to}"),
        ),
        SkillError::Blocked { findings, fields } => ToolOutcome::err(
            "secret_scanner_blocked",
            format!(
                "secret scanner blocked the write: {findings} finding(s) in {} field(s) ({fields:?}). Use {{{{vault:NAME}}}} to reference credentials by name instead of inlining them.",
                fields.len()
            ),
        ),
        SkillError::InvalidFrontmatter(m) => {
            ToolOutcome::err("invalid_argument", format!("frontmatter must be JSON: {m}"))
        }
        SkillError::ResourceTooLarge { size, cap } => ToolOutcome::err(
            "resource_too_large",
            format!("resource size {size} exceeds cap {cap}"),
        ),
        SkillError::BodyTooLarge { size, cap } => ToolOutcome::err(
            "body_too_large",
            format!("body size {size} exceeds cap {cap}"),
        ),
        SkillError::Denied(r) => ToolOutcome::denied(r),
        SkillError::Db(e) => ToolOutcome::err("storage_error", e.to_string()),
    }
}

// ----------------------------------------------------------------
// skills.list
// ----------------------------------------------------------------

pub struct SkillsListTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsListTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.list".into(),
                description:
                    "List every available skill (name, description, state, version). Skills are \
                     procedural-knowledge documents that explain how to perform specific tasks. \
                     Read this first; if a description matches the user's request, call \
                     `skills.view(name)` to load the full instructions."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_read(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsListTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, _ctx: ToolCtx, _args: Value) -> ToolOutcome {
        match self.store.list_index() {
            Ok(entries) => ToolOutcome::ok(json!({ "skills": entries })),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.view
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct ViewArgs {
    name: String,
}

pub struct SkillsViewTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsViewTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.view".into(),
                description:
                    "Load the full body of a skill into context. Returns the markdown \
                     instructions plus a list of bundled resource paths (fetch each via \
                     `skills.resource(name, path)`). Calling this records an activation \
                     event used for skill-usage analytics."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Skill name (e.g. \"research/gather-sources\")."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_read(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsViewTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ViewArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        match self.store.view(&args.name) {
            Ok(Some(view)) => {
                // Record activation; failure here doesn't fail the
                // view (analytics shouldn't block agent progress) but
                // we DO log it so a persistent failure isn't silent.
                if let Err(e) = self.store.record_invocation(
                    &args.name,
                    ctx.conversation_id.as_str(),
                    ctx.clock.now_unix() * 1000,
                ) {
                    tracing::warn!(
                        skill = %args.name,
                        conversation_id = %ctx.conversation_id.as_str(),
                        error = %e,
                        "skills.view: record_invocation failed (analytics only; view returned successfully)"
                    );
                }
                ToolOutcome::ok(serde_json::to_value(&view).unwrap_or(Value::Null))
            }
            Ok(None) => ToolOutcome::err("not_found", format!("no such skill: {}", args.name)),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.resource
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct ResourceArgs {
    name: String,
    path: String,
}

pub struct SkillsResourceTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsResourceTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.resource".into(),
                description:
                    "Fetch a bundled resource attached to a skill's current version. Text-y \
                     resources are returned inline as UTF-8; binary resources are returned \
                     base64-encoded. Use `skills.view` first to see the available paths."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "path": {"type": "string"}
                    },
                    "required": ["name", "path"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_read(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsResourceTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, _ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ResourceArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        match self.store.resource(&args.name, &args.path) {
            Ok(Some(r)) => ToolOutcome::ok(serde_json::to_value(&r).unwrap_or(Value::Null)),
            Ok(None) => ToolOutcome::err(
                "not_found",
                format!("no such resource: {}/{}", args.name, args.path),
            ),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.search
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    10
}

pub struct SkillsSearchTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsSearchTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.search".into(),
                description:
                    "Full-text search across skill descriptions and bodies. Returns the top-K \
                     matches ranked by relevance. Use this when the skill catalog is large; \
                     for a small catalog `skills.list` is sufficient."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_read(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsSearchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, _ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: SearchArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        match self.store.search(&args.query, args.limit) {
            Ok(hits) => ToolOutcome::ok(json!({ "matches": hits })),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.create  (admin-only)
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct CreateArgs {
    name: String,
    description: String,
    body_md: String,
    #[serde(default = "default_frontmatter")]
    frontmatter_json: String,
    #[serde(default)]
    resources: Vec<CreateResourceArg>,
    #[serde(default)]
    promotion_notes: Option<String>,
}

#[derive(Deserialize)]
struct CreateResourceArg {
    path: String,
    mime: String,
    /// UTF-8 inline text. For binary resources use `body_b64` instead.
    #[serde(default)]
    body_text: Option<String>,
    /// Base64-encoded bytes. Mutually exclusive with `body_text`.
    #[serde(default)]
    body_b64: Option<String>,
}

fn default_frontmatter() -> String {
    "{}".into()
}

pub struct SkillsCreateTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsCreateTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.create".into(),
                description:
                    "Author a new skill (admin only). The skill is created in `trial` state; \
                     promote to `stable` via `skills.promote`. Name must be \
                     `<namespace>/<name>` (lowercase a-z, 0-9, hyphen). Reference credentials \
                     using `{{vault:NAME}}` — never inline secrets, the secret scanner will \
                     reject the write."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "body_md": {"type": "string"},
                        "frontmatter_json": {"type": "string", "default": "{}"},
                        "resources": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string"},
                                    "mime": {"type": "string"},
                                    "body_text": {"type": "string"},
                                    "body_b64": {"type": "string"}
                                },
                                "required": ["path", "mime"],
                                "additionalProperties": false
                            }
                        },
                        "promotion_notes": {"type": "string"}
                    },
                    "required": ["name", "description", "body_md"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_admin_write(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsCreateTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        if let Err(o) = require_admin(&ctx) {
            return o;
        }
        let args: CreateArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        if let Err(e) = validate_skill_name(&args.name) {
            return skill_error_to_outcome(e);
        }
        let mut resources = Vec::with_capacity(args.resources.len());
        for r in args.resources {
            let bytes = match (r.body_text, r.body_b64) {
                (Some(t), None) => t.into_bytes(),
                (None, Some(b)) => match base64_decode(&b) {
                    Ok(v) => v,
                    Err(e) => return ToolOutcome::err("invalid_argument", e),
                },
                (Some(_), Some(_)) => {
                    return ToolOutcome::err(
                        "invalid_argument",
                        format!("resource {:?} sets both body_text and body_b64", r.path),
                    );
                }
                (None, None) => Vec::new(),
            };
            resources.push(ResourceBlob {
                path: r.path,
                mime: r.mime,
                bytes,
            });
        }
        let new = NewSkill {
            name: args.name.clone(),
            source: format!("admin:{}", ctx.caller_trust),
            registration_kind: RegistrationKind::Authored,
            owning_plugin_id: None,
            initial_version: NewSkillVersion {
                description: args.description,
                body_md: args.body_md,
                frontmatter_json: args.frontmatter_json,
                authored_by: format!("admin:{}", ctx.caller_trust),
                promotion_notes: args.promotion_notes,
            },
            resources,
        };
        let now_ms = ctx.clock.now_unix() * 1000;
        // Admin writes use Strict by default per the locked design.
        // Admin can re-author with a vault reference if a false
        // positive blocks them.
        match self.store.create(new, Strictness::Strict, now_ms) {
            Ok(id) => ToolOutcome::ok(json!({ "skill_id": id.raw(), "name": args.name })),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.update  (admin-only)
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct UpdateArgs {
    name: String,
    description: String,
    body_md: String,
    #[serde(default = "default_frontmatter")]
    frontmatter_json: String,
    #[serde(default)]
    promotion_notes: Option<String>,
}

pub struct SkillsUpdateTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsUpdateTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.update".into(),
                description:
                    "Add a new version to an existing skill (admin only). The new version \
                     becomes the current one; the previous version is preserved in history."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "body_md": {"type": "string"},
                        "frontmatter_json": {"type": "string", "default": "{}"},
                        "promotion_notes": {"type": "string"}
                    },
                    "required": ["name", "description", "body_md"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_admin_write(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsUpdateTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        if let Err(o) = require_admin(&ctx) {
            return o;
        }
        let args: UpdateArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let v = NewSkillVersion {
            description: args.description,
            body_md: args.body_md,
            frontmatter_json: args.frontmatter_json,
            authored_by: format!("admin:{}", ctx.caller_trust),
            promotion_notes: args.promotion_notes,
        };
        let now_ms = ctx.clock.now_unix() * 1000;
        match self.store.add_version(&args.name, v, Strictness::Strict, now_ms) {
            Ok(id) => ToolOutcome::ok(json!({ "version_id": id.raw(), "name": args.name })),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.promote  (admin-only)
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct PromoteArgs {
    name: String,
    #[serde(default)]
    notes: Option<String>,
}

pub struct SkillsPromoteTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsPromoteTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.promote".into(),
                description:
                    "Promote a skill from `trial` to `stable` (admin only). Idempotent: \
                     promoting an already-stable skill is a no-op."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "notes": {"type": "string"}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_admin_write(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsPromoteTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        if let Err(o) = require_admin(&ctx) {
            return o;
        }
        let args: PromoteArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let now_ms = ctx.clock.now_unix() * 1000;
        match self.store.promote(&args.name, args.notes, now_ms) {
            Ok(()) => ToolOutcome::ok(json!({ "name": args.name, "ok": true })),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// skills.archive  (admin-only)
// ----------------------------------------------------------------

#[derive(Deserialize)]
struct ArchiveArgs {
    name: String,
}

pub struct SkillsArchiveTool {
    descriptor: ToolDescriptor,
    store: Arc<SkillStore>,
}

impl SkillsArchiveTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: "skills.archive".into(),
                description:
                    "Retire a skill (admin only). Archived skills are invisible to the agent \
                     surface; their version history is preserved for audit. Idempotent."
                        .into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: classes_for_admin_write(),
            },
            store,
        }
    }
}

#[async_trait]
impl ToolImpl for SkillsArchiveTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        if let Err(o) = require_admin(&ctx) {
            return o;
        }
        let args: ArchiveArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let now_ms = ctx.clock.now_unix() * 1000;
        match self.store.archive(&args.name, now_ms) {
            Ok(()) => ToolOutcome::ok(json!({ "name": args.name, "ok": true })),
            Err(e) => skill_error_to_outcome(e),
        }
    }
}

// ----------------------------------------------------------------
// Bundled registration
// ----------------------------------------------------------------

/// All seven skill tools, ready to drop into the host's `HookRegistry`
/// alongside `core_builtin_tools()`. The store is shared across all of
/// them via `Arc`.
pub fn skill_tools(store: Arc<SkillStore>) -> Vec<Arc<dyn ToolImpl>> {
    vec![
        Arc::new(SkillsListTool::new(store.clone())),
        Arc::new(SkillsViewTool::new(store.clone())),
        Arc::new(SkillsResourceTool::new(store.clone())),
        Arc::new(SkillsSearchTool::new(store.clone())),
        Arc::new(SkillsCreateTool::new(store.clone())),
        Arc::new(SkillsUpdateTool::new(store.clone())),
        Arc::new(SkillsPromoteTool::new(store.clone())),
        Arc::new(SkillsArchiveTool::new(store)),
    ]
}

// ----------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    // Tiny standalone base64 decoder so we don't grow direct deps.
    // Accepts standard alphabet with optional `=` padding; whitespace
    // stripped.
    const INVALID: u8 = 0xFF;
    let mut table = [INVALID; 256];
    for (i, b) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[*b as usize] = i as u8;
    }
    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let mut bytes = cleaned.as_slice();
    while bytes.last() == Some(&b'=') {
        bytes = &bytes[..bytes.len() - 1];
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for b in bytes {
        let v = table[*b as usize];
        if v == INVALID {
            return Err(format!("invalid base64 character: {:?}", *b as char));
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::ids::ConversationId;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::tool::SystemClock;

    fn fresh() -> Arc<SkillStore> {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        Arc::new(SkillStore::new(db))
    }

    fn ctx_for(trust: &str) -> ToolCtx {
        ToolCtx::empty(
            ConversationId::from("conv-1"),
            trust,
            Arc::new(SystemClock),
        )
    }

    fn create_args(name: &str, body: &str) -> Value {
        json!({
            "name": name,
            "description": "test",
            "body_md": body,
            "frontmatter_json": "{}"
        })
    }

    // --- list / view roundtrip ---

    #[tokio::test]
    async fn create_then_list_then_view() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let list = SkillsListTool::new(store.clone());
        let view = SkillsViewTool::new(store.clone());

        let r = create
            .invoke(ctx_for("Controller"), create_args("a/x", "the body"))
            .await;
        assert!(matches!(r, ToolOutcome::Ok(_)), "create failed: {r:?}");

        let r = list.invoke(ctx_for("Controller"), json!({})).await;
        let v = match r {
            ToolOutcome::Ok(v) => v,
            other => panic!("list failed: {other:?}"),
        };
        let arr = v["skills"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "a/x");

        let r = view.invoke(ctx_for("Controller"), json!({"name": "a/x"})).await;
        let v = match r {
            ToolOutcome::Ok(v) => v,
            other => panic!("view failed: {other:?}"),
        };
        assert_eq!(v["body_md"], "the body");
    }

    // --- admin gate ---

    #[tokio::test]
    async fn non_admin_cannot_create() {
        let store = fresh();
        let create = SkillsCreateTool::new(store);
        let r = create
            .invoke(ctx_for("KnownTrusted"), create_args("a/no", "body"))
            .await;
        assert!(matches!(r, ToolOutcome::Denied { .. }), "expected Denied, got {r:?}");
    }

    #[tokio::test]
    async fn non_admin_cannot_promote() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let promote = SkillsPromoteTool::new(store);
        // Admin first so the row exists.
        let _ = create
            .invoke(ctx_for("Controller"), create_args("a/p", "body"))
            .await;
        let r = promote
            .invoke(ctx_for("Delegated"), json!({"name": "a/p"}))
            .await;
        assert!(matches!(r, ToolOutcome::Denied { .. }));
    }

    #[tokio::test]
    async fn non_admin_can_read_but_not_write() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let view = SkillsViewTool::new(store.clone());
        let _ = create
            .invoke(ctx_for("Controller"), create_args("a/r", "body"))
            .await;
        let r = view
            .invoke(ctx_for("KnownLimited"), json!({"name": "a/r"}))
            .await;
        assert!(matches!(r, ToolOutcome::Ok(_)));
    }

    // --- secret scanner integration via the tool surface ---

    #[tokio::test]
    async fn admin_create_with_credential_in_body_is_blocked_with_error_code() {
        let store = fresh();
        let create = SkillsCreateTool::new(store);
        let r = create
            .invoke(
                ctx_for("Controller"),
                create_args("a/leak", "use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to call"),
            )
            .await;
        match r {
            ToolOutcome::Err { code, message } => {
                assert_eq!(code, "secret_scanner_blocked");
                assert!(message.contains("vault"), "error message should hint at vault syntax");
            }
            other => panic!("expected scanner block, got {other:?}"),
        }
    }

    // --- adversarial: model-supplied "trust class" cannot escalate ---

    #[tokio::test]
    async fn create_args_cannot_smuggle_trust_class_field() {
        // The schema rejects unknown fields (additionalProperties=false
        // at the schema layer; the deserializer ignores them anyway
        // because CreateArgs has #[derive(Deserialize)] without
        // deny_unknown_fields). The key invariant is: even if extra
        // fields are present, the caller's trust class comes from
        // ctx.caller_trust, which the model can't influence.
        let store = fresh();
        let create = SkillsCreateTool::new(store);
        let smuggled = json!({
            "name": "a/x",
            "description": "test",
            "body_md": "body",
            "frontmatter_json": "{}",
            "caller_trust": "Controller",
            "trust_class": "Controller"
        });
        let r = create.invoke(ctx_for("KnownTrusted"), smuggled).await;
        assert!(
            matches!(r, ToolOutcome::Denied { .. } | ToolOutcome::Err { .. }),
            "smuggled trust class must not escalate; got {r:?}"
        );
    }

    // --- promote / archive ---

    #[tokio::test]
    async fn promote_then_archive_changes_state_visibly() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let promote = SkillsPromoteTool::new(store.clone());
        let archive = SkillsArchiveTool::new(store.clone());
        let list = SkillsListTool::new(store);
        let _ = create
            .invoke(ctx_for("Controller"), create_args("a/lifecycle", "body"))
            .await;
        let _ = promote
            .invoke(ctx_for("Controller"), json!({"name":"a/lifecycle"}))
            .await;
        let v = list.invoke(ctx_for("Controller"), json!({})).await;
        let val = if let ToolOutcome::Ok(v) = v { v } else { panic!() };
        assert_eq!(val["skills"][0]["state"], "stable");
        let _ = archive
            .invoke(ctx_for("Controller"), json!({"name":"a/lifecycle"}))
            .await;
        let v = list.invoke(ctx_for("Controller"), json!({})).await;
        let val = if let ToolOutcome::Ok(v) = v { v } else { panic!() };
        assert_eq!(val["skills"].as_array().unwrap().len(), 0);
    }

    // --- registration helper ---

    #[test]
    fn skill_tools_returns_all_seven() {
        let store = fresh();
        let tools = skill_tools(store);
        let names: Vec<_> = tools.iter().map(|t| t.descriptor().name.clone()).collect();
        assert_eq!(
            names,
            vec![
                "skills.list",
                "skills.view",
                "skills.resource",
                "skills.search",
                "skills.create",
                "skills.update",
                "skills.promote",
                "skills.archive",
            ]
        );
    }

    // --- base64 helper ---

    #[test]
    fn base64_decode_handles_padding_and_no_padding() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello"); // missing pad
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
    }

    #[test]
    fn base64_decode_rejects_invalid() {
        assert!(base64_decode("!!!").is_err());
    }

    // --- update happy path ---

    #[tokio::test]
    async fn admin_update_advances_version_and_replaces_body() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let update = SkillsUpdateTool::new(store.clone());
        let view = SkillsViewTool::new(store.clone());
        let _ = create
            .invoke(ctx_for("Controller"), create_args("a/u", "v1 body"))
            .await;
        let r = update
            .invoke(
                ctx_for("Controller"),
                json!({
                    "name": "a/u",
                    "description": "v2 desc",
                    "body_md": "v2 body",
                    "frontmatter_json": "{}"
                }),
            )
            .await;
        assert!(matches!(r, ToolOutcome::Ok(_)), "update failed: {r:?}");
        let r = view.invoke(ctx_for("Controller"), json!({"name": "a/u"})).await;
        let v = if let ToolOutcome::Ok(v) = r { v } else { panic!() };
        assert_eq!(v["body_md"], "v2 body");
        assert_eq!(v["version"], 2);
    }

    #[tokio::test]
    async fn admin_update_unknown_skill_returns_storage_error() {
        let store = fresh();
        let update = SkillsUpdateTool::new(store);
        let r = update
            .invoke(
                ctx_for("Controller"),
                json!({"name": "ghost/skill", "description": "x", "body_md": "x"}),
            )
            .await;
        assert!(matches!(r, ToolOutcome::Err { .. }));
    }

    #[tokio::test]
    async fn non_admin_cannot_update() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let update = SkillsUpdateTool::new(store);
        let _ = create
            .invoke(ctx_for("Controller"), create_args("a/u", "v1"))
            .await;
        let r = update
            .invoke(
                ctx_for("KnownTrusted"),
                json!({"name":"a/u","description":"x","body_md":"x"}),
            )
            .await;
        assert!(matches!(r, ToolOutcome::Denied { .. }));
    }

    // --- resource paths ---

    #[tokio::test]
    async fn resource_not_found_returns_not_found_error() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let resource = SkillsResourceTool::new(store);
        let _ = create
            .invoke(ctx_for("Controller"), create_args("a/r", "body"))
            .await;
        let r = resource
            .invoke(
                ctx_for("Controller"),
                json!({"name": "a/r", "path": "missing.txt"}),
            )
            .await;
        match r {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "not_found"),
            other => panic!("expected not_found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resource_for_unknown_skill_returns_not_found() {
        let store = fresh();
        let resource = SkillsResourceTool::new(store);
        let r = resource
            .invoke(
                ctx_for("Controller"),
                json!({"name": "ghost/x", "path": "a.txt"}),
            )
            .await;
        match r {
            ToolOutcome::Err { code, .. } => assert_eq!(code, "not_found"),
            other => panic!("expected not_found, got {other:?}"),
        }
    }

    // --- search ---

    #[tokio::test]
    async fn search_returns_matches_for_keyword_in_description() {
        let store = fresh();
        let create = SkillsCreateTool::new(store.clone());
        let search = SkillsSearchTool::new(store);
        let _ = create
            .invoke(
                ctx_for("Controller"),
                json!({
                    "name": "research/sources",
                    "description": "Use this when gathering web research sources for a question",
                    "body_md": "step 1 ..."
                }),
            )
            .await;
        let r = search
            .invoke(ctx_for("Controller"), json!({"query": "research sources"}))
            .await;
        let v = if let ToolOutcome::Ok(v) = r { v } else { panic!() };
        let matches = v["matches"].as_array().unwrap();
        assert!(!matches.is_empty(), "expected hits: {v:?}");
        assert_eq!(matches[0]["name"], "research/sources");
    }
}
