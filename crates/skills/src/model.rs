//! Type definitions for the skill subsystem.
//!
//! These are the data shapes the store CRUDs and the tools serialize
//! to/from JSON. Everything is plain `serde` so a future REST/Swagger
//! surface picks them up without further work.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Newtype around the auto-increment `state_skills.id`. Opaque to
/// callers; stable across the row's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SkillId(pub i64);

impl SkillId {
    pub fn raw(self) -> i64 {
        self.0
    }
}

/// Newtype around the auto-increment `state_skill_versions.id`. Stable
/// across the version's lifetime; immutable once written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionId(pub i64);

impl VersionId {
    pub fn raw(self) -> i64 {
        self.0
    }
}

/// Lifecycle state of a skill row. `draft` from the v0.1 design was
/// dropped in v0.2 — without a click gate it added no value, so every
/// new skill goes straight to `trial` and admin-promotes to `stable`
/// when ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    Trial,
    Stable,
    Archived,
}

impl SkillState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trial => "trial",
            Self::Stable => "stable",
            Self::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "trial" => Some(Self::Trial),
            "stable" => Some(Self::Stable),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

/// How the skill row got into the store.
///
/// - `Authored` — written by an admin (or, in Phase C, an agent acting
///   on behalf of an admin) via `skills.create`.
/// - `Shipped` — imported from a plugin's ZIP `skills/` directory at
///   plugin install time. Phase B.
/// - `Registered` — registered at runtime by a plugin via
///   `PluginContext::register_skill`. Phase B. Auto-archived when the
///   owning plugin is uninstalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationKind {
    Authored,
    Shipped,
    Registered,
}

impl RegistrationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::Shipped => "shipped",
            Self::Registered => "registered",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "authored" => Some(Self::Authored),
            "shipped" => Some(Self::Shipped),
            "registered" => Some(Self::Registered),
            _ => None,
        }
    }
}

/// A skill row + its current version's surface metadata. Returned by
/// reads that need the full picture (e.g. the admin UI listing). The
/// LLM-facing `skills.list` tool returns the leaner [`SkillIndexEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub state: SkillState,
    pub source: String,
    pub registration_kind: RegistrationKind,
    pub owning_plugin_id: Option<String>,
    pub current_version: SkillVersion,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
}

/// One version of a skill. Immutable once written: edits create a new
/// row in `state_skill_versions` and advance `state_skills.current_version_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillVersion {
    pub id: VersionId,
    pub skill_id: SkillId,
    pub version: u32,
    pub description: String,
    pub body_md: String,
    pub frontmatter_json: String,
    pub body_sha256: String,
    pub authored_by: String,
    pub authored_at: i64,
    pub promotion_notes: Option<String>,
    pub parent_version_id: Option<VersionId>,
}

/// Lean projection used by the LLM-facing `skills.list` tool. Carries
/// only what the model needs to decide whether to activate the skill —
/// the full body comes via `skills.view`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillIndexEntry {
    pub name: String,
    pub description: String,
    pub state: SkillState,
    pub version: u32,
}

/// FTS5 search hit returned by the (Phase D) `skills.search` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMatch {
    pub name: String,
    pub description: String,
    pub state: SkillState,
    pub version: u32,
    /// Lower is a closer match (FTS5 `bm25()` is monotonically
    /// increasing in distance).
    pub rank: f64,
}

/// Activation payload returned by `skills.view` — the full body the
/// model will read into context as instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    pub state: SkillState,
    pub version: u32,
    pub body_md: String,
    pub frontmatter_json: String,
    /// Paths of bundled resources, fetchable via `skills.resource`.
    pub resource_paths: Vec<String>,
}

/// Bundled resource (script, schema, example, etc.) attached to a
/// specific skill version. Returned by `skills.resource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillResource {
    pub path: String,
    pub mime: String,
    pub size_bytes: u64,
    /// UTF-8 body when `mime` is text-y; base64-encoded bytes
    /// otherwise.
    pub body: ResourceBody,
}

/// Body shape returned by [`SkillResource`]. Text bodies are inlined as
/// strings for cheap LLM consumption; binary bodies are base64-encoded
/// so the JSON wire format stays clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum ResourceBody {
    Text { content: String },
    Base64 { content: String },
}

/// Input to [`crate::store::SkillStore::create`].
#[derive(Debug, Clone)]
pub struct NewSkill {
    pub name: String,
    pub source: String,
    pub registration_kind: RegistrationKind,
    pub owning_plugin_id: Option<String>,
    pub initial_version: NewSkillVersion,
    /// Optional bundled resources, attached to the initial version.
    pub resources: Vec<ResourceBlob>,
}

/// Input to [`crate::store::SkillStore::add_version`] (and bundled
/// inside [`NewSkill`] for the first version).
#[derive(Debug, Clone)]
pub struct NewSkillVersion {
    pub description: String,
    pub body_md: String,
    pub frontmatter_json: String,
    pub authored_by: String,
    pub promotion_notes: Option<String>,
}

/// Bytes-level resource attached to a skill version.
#[derive(Debug, Clone)]
pub struct ResourceBlob {
    pub path: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Phase D.1 — newtype around `state_skill_proposals.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProposalId(pub i64);

impl ProposalId {
    pub fn raw(self) -> i64 {
        self.0
    }
}

/// Distinguishes auto-capture (new skill) from reuse-update
/// (version fork of an existing skill).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    NewSkill,
    VersionFork,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewSkill => "new_skill",
            Self::VersionFork => "version_fork",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "new_skill" => Some(Self::NewSkill),
            "version_fork" => Some(Self::VersionFork),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Approved,
    Rejected,
    Superseded,
}

impl ProposalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// Phase D.1 — proposal row read out of `state_skill_proposals`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProposal {
    pub id: ProposalId,
    pub kind: ProposalKind,
    pub target_skill_id: Option<SkillId>,
    pub proposed_name: String,
    pub description: String,
    pub body_md: String,
    pub frontmatter_json: String,
    pub source_run_id: String,
    pub trajectory_summary: Option<String>,
    pub tool_calls_observed: u32,
    pub state: ProposalState,
    pub promoted_skill_id: Option<SkillId>,
    pub promoted_version_id: Option<VersionId>,
    pub created_at: i64,
    pub reviewed_at: Option<i64>,
    pub reviewer: Option<String>,
    pub decision_notes: Option<String>,
}

/// Input to [`crate::store::SkillStore::submit_proposal`].
#[derive(Debug, Clone)]
pub struct NewProposal {
    pub kind: ProposalKind,
    pub target_skill_id: Option<SkillId>,
    pub proposed_name: String,
    pub description: String,
    pub body_md: String,
    pub frontmatter_json: String,
    pub source_run_id: String,
    pub trajectory_summary: Option<String>,
    pub tool_calls_observed: u32,
}

/// Errors the skill subsystem returns. Wraps the core DB errors plus
/// scanner verdicts plus model-level invariant violations.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error(transparent)]
    Db(#[from] execlaw_core::db::DbError),

    #[error("invalid skill name {0:?}: must match `<namespace>/<name>` (lowercase a-z, 0-9, hyphen)")]
    InvalidName(String),

    #[error("skill not found: {0}")]
    NotFound(String),

    #[error("skill already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid state transition: {from} -> {to}")]
    InvalidStateTransition { from: String, to: String },

    #[error("secret scanner blocked write: {findings} finding(s) at {fields:?}")]
    Blocked {
        findings: usize,
        fields: Vec<String>,
    },

    #[error("frontmatter is not valid JSON: {0}")]
    InvalidFrontmatter(String),

    #[error("resource size {size} exceeds cap {cap}")]
    ResourceTooLarge { size: u64, cap: u64 },

    #[error("skill body size {size} exceeds cap {cap}")]
    BodyTooLarge { size: u64, cap: u64 },

    #[error("permission denied: {0}")]
    Denied(String),
}

/// Compile-time caps applied by the store before any DB write.
/// Locked at 2026-05-02. Configurable in a future phase via
/// `config_runtime_settings`.
pub const MAX_BODY_BYTES: u64 = 256 * 1024;
pub const MAX_RESOURCE_BYTES: u64 = 1024 * 1024;
pub const MAX_SKILL_TOTAL_BYTES: u64 = 10 * 1024 * 1024;

/// Validate a skill name against the `<namespace>/<name>` convention.
/// Locked at 2026-05-02:
///   * exactly one `/`
///   * each segment matches `[a-z0-9][a-z0-9-]*`
///   * total length 3..=128 chars
pub fn validate_skill_name(name: &str) -> Result<(), SkillError> {
    if name.len() < 3 || name.len() > 128 {
        return Err(SkillError::InvalidName(name.to_string()));
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() != 2 {
        return Err(SkillError::InvalidName(name.to_string()));
    }
    for seg in &parts {
        if seg.is_empty() {
            return Err(SkillError::InvalidName(name.to_string()));
        }
        let mut chars = seg.chars();
        let first = chars.next().unwrap();
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(SkillError::InvalidName(name.to_string()));
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                return Err(SkillError::InvalidName(name.to_string()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_state_parse_roundtrips() {
        for s in [SkillState::Trial, SkillState::Stable, SkillState::Archived] {
            assert_eq!(SkillState::parse(s.as_str()), Some(s));
        }
        assert_eq!(SkillState::parse("nope"), None);
    }

    #[test]
    fn registration_kind_parse_roundtrips() {
        for k in [
            RegistrationKind::Authored,
            RegistrationKind::Shipped,
            RegistrationKind::Registered,
        ] {
            assert_eq!(RegistrationKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(RegistrationKind::parse("garbage"), None);
    }

    #[test]
    fn validate_skill_name_accepts_canonical_form() {
        validate_skill_name("research/gather-sources").unwrap();
        validate_skill_name("dev/scaffold-rust-crate").unwrap();
        validate_skill_name("a/b").unwrap();
        validate_skill_name("plug1n/n4me").unwrap();
    }

    #[test]
    fn validate_skill_name_rejects_uppercase_punctuation_and_double_slash() {
        for bad in [
            "Research/Sources",          // uppercase
            "research/gather sources",   // space
            "research//double",          // empty middle segment
            "/leading-slash",            // empty first segment
            "trailing/",                 // empty last segment
            "no-slash-at-all",           // no slash
            "too/many/slashes",          // multiple slashes
            "-leading-hyphen/name",      // leading hyphen
            "name/-leading-hyphen",      // leading hyphen in second
            "a/b!",                      // punctuation
            "ab",                        // too short
            &"x".repeat(200),            // too long
        ] {
            assert!(
                validate_skill_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
