//! DB-backed skill subsystem for execlaw.
//!
//! A skill is a versioned document of procedural knowledge — a markdown
//! body plus structured frontmatter and zero-or-more bundled resources —
//! that the agent loads on demand to shape its behavior. Skills are
//! advice to the model, not executable callables; the actual tool calls
//! a skill provokes still flow through the existing tool dispatcher and
//! its per-tool trust gating.
//!
//! ## Phase A scope (this crate, today)
//!
//! - Schema (migration `0029_skills.sql`)
//! - [`store::SkillStore`] — pure CRUD against SQLite, with version
//!   monotonicity, blob refcounting, and FTS5 index sync.
//! - [`scanner::scan`] — deterministic in-process secret scanner that
//!   guards every write path. Catches the common accidental-leakage
//!   cases (provider API keys, PEM private keys, JWTs, URLs with inline
//!   credentials, high-entropy random strings) without making any
//!   network calls or invoking a model.
//! - [`tools`] — the model-facing tool surface:
//!     - read tools (always available): `skills.list`, `skills.view`,
//!       `skills.resource`
//!     - admin-gated write tools: `skills.create`, `skills.update`,
//!       `skills.promote`, `skills.archive`
//!
//! ## What's NOT in Phase A
//!
//! - Plugin registration (`PluginContext::register_skill`) — Phase B.
//! - Auto-capture worker (≥5-tool-call trajectory summarization) — Phase C.
//! - Admin UI editor / version diff view — Phase D.
//! - DSPy/GEPA-style offline optimizer — placeholder; reserved for
//!   `state_skill_eval_runs` (not yet migrated).
//!
//! See `MEMORY.md` and the design conversation locked at 2026-05-02 for
//! the rationale behind every shape choice.

pub mod capture;
pub mod import;
pub mod model;
pub mod reuse_update;
pub mod sanitizer;
pub mod scanner;
pub mod store;
pub mod summarizer;
pub mod tools;

pub use capture::{AutoCaptureSink, AutoCaptureWorker, CaptureOutcome, CaptureRequest};
pub use import::{
    ImportFailure, ImportReport, import_plugin_skills, namespaced_name, sanitize_local_name,
};
pub use model::{
    MAX_BODY_BYTES, MAX_RESOURCE_BYTES, MAX_SKILL_TOTAL_BYTES, NewProposal, NewSkill,
    NewSkillVersion, ProposalId, ProposalKind, ProposalState, RegistrationKind, ResourceBlob,
    Skill, SkillError, SkillId, SkillIndexEntry, SkillMatch, SkillProposal, SkillResource,
    SkillState, SkillVersion, SkillView, VersionId,
};
pub use reuse_update::{ReuseUpdateRequest, ReuseUpdateSink, ReuseUpdateWorker};
pub use sanitizer::{SanitizationReport, SanitizedStep, sanitize_step};
pub use scanner::{Finding, FindingKind, ScanInput, ScanVerdict, Severity, Strictness, scan};
pub use store::SkillStore;
pub use summarizer::{
    DraftSkillProposal, SkillSummarizer, SummarizerOutput, SummarizerPrompt,
    build_improvement_prompt, build_prompt, parse_response,
};
pub use tools::skill_tools;
