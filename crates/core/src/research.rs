//! Deep-research job store + per-row vocabulary (§2.9.1, C3+).
//!
//! Three-table model lives in migration 0027:
//!
//!   * `state_research_jobs` — durable per-job row. The
//!     [`ResearchJobStore`] CRUD wraps it.
//!   * `config_research` — singleton operator-editable defaults.
//!     [`ResearchConfigStore`] handles read/write; the
//!     `/api/admin/settings/research` endpoint pair drives it.
//!   * Workspace dirs on disk (`~/.execlaw/research/<job_id>/`) hold
//!     bulky payloads; only the index lives in SQLite.
//!
//! The runner ([`crate::server::research::runner`] in C3, with the
//! gather + synthesize phases landing in C4-C5) reads the row, makes
//! the LLM calls, writes notes/report to the workspace, emits Card
//! events, and flips status atomically.
//!
//! 2026-04-29.

use crate::db::{Database, DbError};
use crate::ids::{ConversationId, ResearchJobId};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// -----------------------------------------------------------------
// Lifecycle vocabulary
// -----------------------------------------------------------------

/// State machine for a research job. Transitions are linear up to
/// `Complete`; `Cancelled` and `Failed` are terminal exits from any
/// in-flight state.
///
/// C3 only ever drives `Pending → Planning → Planned`. C4 adds
/// `Planned → Gathering → Synthesizing`. C5 lands `Synthesizing →
/// Complete` with the final report attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResearchJobStatus {
    /// Just inserted; supervisor hasn't picked it up yet.
    Pending,
    /// Runner has the row and is making the planner LLM call.
    Planning,
    /// Plan landed; awaiting `phase_gates` resolution before gather.
    Planned,
    /// Gather workers are running (C4).
    Gathering,
    /// Single synthesize LLM call composing the report (C5).
    Synthesizing,
    /// Terminal: report written + attachment_id set.
    Complete,
    /// Terminal: runner reported a failure; `error` populated.
    Failed,
    /// Terminal: operator cancelled cooperatively.
    Cancelled,
}

impl ResearchJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Planning => "planning",
            Self::Planned => "planned",
            Self::Gathering => "gathering",
            Self::Synthesizing => "synthesizing",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "planning" => Some(Self::Planning),
            "planned" => Some(Self::Planned),
            "gathering" => Some(Self::Gathering),
            "synthesizing" => Some(Self::Synthesizing),
            "complete" => Some(Self::Complete),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Whether the row has reached a terminal state and the runner
    /// will not modify it further. The retention sweeper (C6) only
    /// considers terminal rows for purge.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }
}

/// Phase-gate vocabulary persisted on `config_research.phase_gates`.
/// Locks the operator's preferred level of intervention between
/// phases; the runner's transition logic consults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PhaseGates {
    /// Auto-advance through every phase.
    None,
    /// Pause after `Planning → Planned`; await operator confirm
    /// before `Planned → Gathering`. Default — gives the operator a
    /// one-click confirm before the expensive gather phase fires.
    #[default]
    PlanOnly,
    /// Pause between every phase (C6).
    EveryPhase,
}

impl PhaseGates {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PlanOnly => "plan_only",
            Self::EveryPhase => "every_phase",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "plan_only" => Some(Self::PlanOnly),
            "every_phase" => Some(Self::EveryPhase),
            _ => None,
        }
    }
}

// -----------------------------------------------------------------
// Row + payload types
// -----------------------------------------------------------------

/// One sub-query in a planner's output. The gather phase (C4) spawns
/// one worker per entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Sub-query the worker should run against the search provider.
    pub query: String,
    /// One-line rationale the planner wrote — surfaced in the
    /// ResearchCard renderer so the operator sees why each step
    /// exists.
    #[serde(default)]
    pub rationale: Option<String>,
}

/// Materialised plan written by the planner LLM call. Persisted
/// MessagePack-encoded into `state_research_jobs.plan_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchPlan {
    /// One-paragraph framing the planner used to produce the steps.
    pub thesis: String,
    pub steps: Vec<PlanStep>,
}

/// Per-sub-query state tracked across the gather phase. Persisted
/// inside `ResearchNote` and surfaced through the Card's
/// `details_json` so the SPA's ResearchCard renderer can paint the
/// per-row Pending/Running/Done/Failed badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubQueryState {
    Pending,
    Running,
    Done,
    Failed,
}

impl SubQueryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Done => "Done",
            Self::Failed => "Failed",
        }
    }
}

/// One source the gather worker pulled. The Card renderer surfaces
/// these as a clickable link list under each sub-query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchSource {
    pub url: String,
    pub title: Option<String>,
    /// Whether the fetch succeeded. Failed sources are kept (with a
    /// brief `error` message) so the operator can inspect what went
    /// wrong without digging through logs.
    pub fetched_ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// One gather worker's output, persisted into
/// `state_research_jobs.notes_json` (and as `notes/<n>.json` on
/// disk). The runner appends one `ResearchNote` per `PlanStep` after
/// the per-query subagent extraction returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchNote {
    /// Index into the `ResearchPlan.steps` list. Stable so the SPA
    /// can match notes back to plan rows.
    pub index: u32,
    pub sub_query: String,
    pub state: SubQueryState,
    /// Subagent-extracted facts. Empty when state == Failed.
    pub excerpt: String,
    pub sources: Vec<ResearchSource>,
    /// Tokens the subagent reported. `None` when the inference
    /// backend's usage block is missing.
    #[serde(default)]
    pub tokens_used: Option<u32>,
    /// Operator-safe failure message when state == Failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// Full row as stored in `state_research_jobs`. The runner +
/// admin endpoints + tools read/write this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchJobRow {
    pub id: ResearchJobId,
    pub conversation_id: ConversationId,
    pub query: String,
    pub status: ResearchJobStatus,
    pub caller_trust: String,
    pub card_id: Option<String>,
    pub plan_json: Option<Vec<u8>>,
    pub notes_json: Option<Vec<u8>>,
    pub workspace_path: Option<String>,
    pub attachment_id: Option<String>,
    pub error: Option<String>,
    pub overrides_json: Option<Vec<u8>>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// Compact projection returned by `ResearchJobStore::list_*` and the
/// `research_status` / `research_list` tools. Drops bulky payload
/// blobs so the LLM-facing surface stays small.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchJobSummary {
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
    /// Decoded plan if present, else None. The summary always
    /// carries the plan because it's small (~few-hundred chars) and
    /// the operator UI / tools want to see it as soon as it lands.
    pub plan: Option<ResearchPlan>,
    /// Decoded gather-phase notes. Populated as the gather workers
    /// land their per-sub-query extractions; a partial list during
    /// in-flight gather is fine — the SPA's ResearchCard reads
    /// each note's `state` to paint per-row status badges.
    #[serde(default)]
    pub notes: Vec<ResearchNote>,
}

impl ResearchJobRow {
    /// Compute the summary form. Decoding the plan is best-effort —
    /// a corrupt blob surfaces as `plan: None` rather than failing
    /// the whole query.
    pub fn to_summary(&self) -> ResearchJobSummary {
        ResearchJobSummary {
            id: self.id.as_str().to_owned(),
            conversation_id: self.conversation_id.as_str().to_owned(),
            query: self.query.clone(),
            status: self.status.as_str().to_owned(),
            card_id: self.card_id.clone(),
            workspace_path: self.workspace_path.clone(),
            attachment_id: self.attachment_id.clone(),
            error: self.error.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            plan: self
                .plan_json
                .as_ref()
                .and_then(|b| rmp_serde::from_slice::<ResearchPlan>(b).ok()),
            notes: self
                .notes_json
                .as_ref()
                .and_then(|b| rmp_serde::from_slice::<Vec<ResearchNote>>(b).ok())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ResearchError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("encoding: {0}")]
    Encoding(String),
}

// -----------------------------------------------------------------
// JobStore
// -----------------------------------------------------------------

/// CRUD wrapper for `state_research_jobs`. Cheap to construct
/// (borrows the `Database`); callers should NOT cache them across
/// async boundaries — clone the `Database` and re-construct.
pub struct ResearchJobStore<'db> {
    db: &'db Database,
}

impl<'db> ResearchJobStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a brand-new `Pending` job. Returns the inserted row
    /// (with timestamps populated). The caller mints the id (the
    /// tool's response includes it so the model can reference it
    /// in subsequent `research_status` calls).
    pub fn insert_pending(
        &self,
        id: &ResearchJobId,
        conversation_id: &ConversationId,
        query: &str,
        caller_trust: &str,
        overrides_json: Option<Vec<u8>>,
        now: i64,
    ) -> Result<ResearchJobRow, ResearchError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(ResearchError::Invalid("query is empty".into()));
        }
        if trimmed.chars().count() > 8_000 {
            return Err(ResearchError::Invalid(
                "query too long (max 8000 chars)".into(),
            ));
        }
        let id_owned = id.as_str().to_owned();
        let cid = conversation_id.as_str().to_owned();
        let q = trimmed.to_owned();
        let trust = caller_trust.to_owned();
        let overrides = overrides_json;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_research_jobs \
                   (id, conversation_id, query, status, caller_trust, \
                    overrides_json, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6, ?6)",
                params![id_owned, cid, q, trust, overrides, now],
            )?;
            Ok(())
        })?;
        self.get(id)?
            .ok_or_else(|| ResearchError::NotFound(id.as_str().to_owned()))
    }

    pub fn get(&self, id: &ResearchJobId) -> Result<Option<ResearchJobRow>, ResearchError> {
        let id_owned = id.as_str().to_owned();
        let row = self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT id, conversation_id, query, status, caller_trust, \
                            card_id, plan_json, notes_json, workspace_path, \
                            attachment_id, error, overrides_json, \
                            created_at, updated_at, started_at, finished_at \
                     FROM state_research_jobs WHERE id = ?1",
                    params![id_owned],
                    row_to_research_row,
                )
                .ok();
            Ok(got)
        })?;
        Ok(row)
    }

    /// Pick up the next `Pending` job (oldest first) and atomically
    /// flip its status to `Planning`, recording `started_at` and
    /// (optionally) the supervisor-minted `card_id`. Returns the
    /// claimed row, or `None` when no Pending row exists.
    ///
    /// The atomic claim is what stops two supervisor instances
    /// (process restart races, or a future multi-supervisor topology)
    /// from picking up the same job.
    pub fn claim_next_pending(
        &self,
        card_id: &str,
        now: i64,
    ) -> Result<Option<ResearchJobRow>, ResearchError> {
        let card_id_owned = card_id.to_owned();
        let claimed_id: Option<String> = self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let id: Option<String> = tx
                .query_row(
                    "SELECT id FROM state_research_jobs \
                     WHERE status = 'pending' \
                     ORDER BY created_at ASC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            if let Some(id) = id.as_ref() {
                tx.execute(
                    "UPDATE state_research_jobs \
                     SET status = 'planning', card_id = ?1, \
                         started_at = ?2, updated_at = ?2 \
                     WHERE id = ?3 AND status = 'pending'",
                    params![card_id_owned, now, id],
                )?;
            }
            tx.commit()?;
            Ok(id)
        })?;
        match claimed_id {
            Some(id) => self.get(&ResearchJobId::from(id.as_str())),
            None => Ok(None),
        }
    }

    /// Persist the planner output and flip status to `Planned`.
    pub fn set_planned(
        &self,
        id: &ResearchJobId,
        plan: &ResearchPlan,
        now: i64,
    ) -> Result<(), ResearchError> {
        let blob = rmp_serde::to_vec(plan).map_err(|e| ResearchError::Encoding(e.to_string()))?;
        let id_owned = id.as_str().to_owned();
        // Status guard: a runner that's mid-planner LLM call when
        // the operator cancels would otherwise resurrect the row by
        // overwriting Cancelled with Planned. Same regression
        // pattern as `finish()` (see commit d7ea494). The
        // `WHERE status = 'planning'` predicate makes this a no-op
        // on cancelled / failed rows; the runner observes the
        // 0-row update and exits its phase loop without progressing
        // (the cancel path is what handles the row's lifecycle).
        let updated = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET plan_json = ?1, status = 'planned', updated_at = ?2 \
                 WHERE id = ?3 AND status = 'planning'",
                params![blob, now, id_owned],
            )?;
            Ok(n)
        })?;
        if updated == 0 {
            // Row may have been cancelled or failed during the
            // planner LLM call. Surface as NotFound so the runner
            // treats it as a "row gone away during phase" condition
            // — same code path as a row truly missing, which is
            // the right behaviour (don't proceed to gather).
            return Err(ResearchError::NotFound(id.as_str().to_owned()));
        }
        Ok(())
    }

    /// Flip status from `Planned` → `Gathering`. Atomic on the
    /// status predicate so the supervisor (or a future
    /// operator-driven advance flow) can race-safely transition the
    /// row exactly once. Returns `Ok(false)` when the row was not
    /// in `Planned` — callers can treat that as a no-op.
    pub fn mark_gathering(&self, id: &ResearchJobId, now: i64) -> Result<bool, ResearchError> {
        let id_owned = id.as_str().to_owned();
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET status = 'gathering', updated_at = ?1 \
                 WHERE id = ?2 AND status = 'planned'",
                params![now, id_owned],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Persist the (partial or final) gather-phase notes. Encoded
    /// MessagePack into `notes_json`. Safe to call repeatedly as
    /// per-worker results land — the SPA's ResearchCard then sees
    /// per-sub-query state badges flip from Pending → Running →
    /// Done in real time. Does NOT change `status`; the caller flips
    /// to `Synthesizing` when every worker has reported.
    pub fn set_notes(
        &self,
        id: &ResearchJobId,
        notes: &[ResearchNote],
        now: i64,
    ) -> Result<(), ResearchError> {
        let blob = rmp_serde::to_vec(notes).map_err(|e| ResearchError::Encoding(e.to_string()))?;
        let id_owned = id.as_str().to_owned();
        // Status guard: a gather worker that fired off
        // search/fetch/subagent calls before the cancel landed
        // would otherwise advance `updated_at` on a Cancelled row,
        // causing the operator's `/research` list view (sorted by
        // updated_at) to surface the cancelled job as "more
        // recent" than it really is. Skip the write on terminal
        // rows. NotFound on the runner side stops the phase loop
        // gracefully — same shape as a truly missing row.
        let updated = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET notes_json = ?1, updated_at = ?2 \
                 WHERE id = ?3 \
                   AND status NOT IN ('complete', 'failed', 'cancelled')",
                params![blob, now, id_owned],
            )?;
            Ok(n)
        })?;
        if updated == 0 {
            return Err(ResearchError::NotFound(id.as_str().to_owned()));
        }
        Ok(())
    }

    /// Flip status from `Gathering` → `Synthesizing`. Atomic on the
    /// status predicate. Returns `Ok(false)` when the row was not
    /// in `Gathering`.
    pub fn mark_synthesizing(&self, id: &ResearchJobId, now: i64) -> Result<bool, ResearchError> {
        let id_owned = id.as_str().to_owned();
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET status = 'synthesizing', updated_at = ?1 \
                 WHERE id = ?2 AND status = 'gathering'",
                params![now, id_owned],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Move the row to a terminal state. `error` is required for
    /// `Failed`; for `Complete` callers should also pass the
    /// `attachment_id` for the report.
    ///
    /// Returns `Ok(true)` when the transition landed, `Ok(false)`
    /// when the row was ALREADY terminal — the status guard
    /// prevents a late `finish(Complete)` from silently
    /// overwriting an earlier `Cancelled` (the cancel-overwrite
    /// regression an audit caught after the C6c cancel-token
    /// plumbing landed). Without this guard, the runner's natural
    /// "synthesise complete → finish(Complete)" path could undo
    /// an operator cancel that fired mid-gather, silently
    /// resurrecting a job the operator killed and producing a
    /// rendering-glitch CardClosed sequence on the SPA.
    pub fn finish(
        &self,
        id: &ResearchJobId,
        terminal: ResearchJobStatus,
        error: Option<&str>,
        attachment_id: Option<&str>,
        now: i64,
    ) -> Result<bool, ResearchError> {
        if !terminal.is_terminal() {
            return Err(ResearchError::Invalid(format!(
                "{} is not a terminal status",
                terminal.as_str()
            )));
        }
        let id_owned = id.as_str().to_owned();
        let status = terminal.as_str();
        let error_owned = error.map(str::to_owned);
        let attachment_owned = attachment_id.map(str::to_owned);
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET status = ?1, error = COALESCE(?2, error), \
                     attachment_id = COALESCE(?3, attachment_id), \
                     finished_at = ?4, updated_at = ?4 \
                 WHERE id = ?5 \
                   AND status NOT IN ('complete', 'failed', 'cancelled')",
                params![status, error_owned, attachment_owned, now, id_owned],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Set the workspace directory path on disk (the runner provisions
    /// it during `Pending → Planning`). Skips the write on terminal
    /// rows so a runner that races a cancel doesn't advance
    /// `updated_at` on a Cancelled row.
    pub fn set_workspace_path(
        &self,
        id: &ResearchJobId,
        path: &str,
        now: i64,
    ) -> Result<(), ResearchError> {
        let id_owned = id.as_str().to_owned();
        let path_owned = path.to_owned();
        let updated = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET workspace_path = ?1, updated_at = ?2 \
                 WHERE id = ?3 \
                   AND status NOT IN ('complete', 'failed', 'cancelled')",
                params![path_owned, now, id_owned],
            )?;
            Ok(n)
        })?;
        if updated == 0 {
            // Row missing OR already terminal (a cancel that
            // raced the runner's claim → provision sequence). The
            // runner's phase loop treats NotFound as "exit
            // cleanly", same shape as set_planned / set_notes.
            return Err(ResearchError::NotFound(id.as_str().to_owned()));
        }
        Ok(())
    }

    /// List every job whose `conversation_id` matches, newest first.
    /// Used by the chat-pane "running jobs" badge + `research_list`
    /// when the caller scopes to their own thread.
    pub fn list_for_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<ResearchJobRow>, ResearchError> {
        let cid = conversation_id.as_str().to_owned();
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, query, status, caller_trust, \
                        card_id, plan_json, notes_json, workspace_path, \
                        attachment_id, error, overrides_json, \
                        created_at, updated_at, started_at, finished_at \
                 FROM state_research_jobs WHERE conversation_id = ?1 \
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map(params![cid], row_to_research_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// List every job, newest first. Used by the future /research
    /// page + by Controller-trust `research_list` calls.
    pub fn list_all(&self) -> Result<Vec<ResearchJobRow>, ResearchError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, query, status, caller_trust, \
                        card_id, plan_json, notes_json, workspace_path, \
                        attachment_id, error, overrides_json, \
                        created_at, updated_at, started_at, finished_at \
                 FROM state_research_jobs ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map([], row_to_research_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Atomic cancel — flips any non-terminal row to `Cancelled`,
    /// stamping `finished_at = now`. Returns `Ok(true)` when the
    /// row transitioned, `Ok(false)` when the row was already
    /// terminal (idempotent — a duplicate cancel is a no-op rather
    /// than an error). Used by the C6 admin endpoint and by the
    /// future operator-driven cancel button on the ResearchCard.
    pub fn cancel_active(
        &self,
        id: &ResearchJobId,
        reason: Option<&str>,
        now: i64,
    ) -> Result<bool, ResearchError> {
        let id_owned = id.as_str().to_owned();
        let reason_owned = reason.map(str::to_owned);
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_research_jobs \
                 SET status = 'cancelled', \
                     error = COALESCE(?1, error), \
                     finished_at = ?2, \
                     updated_at = ?2 \
                 WHERE id = ?3 \
                   AND status IN ('pending', 'planning', 'planned', \
                                  'gathering', 'synthesizing')",
                params![reason_owned, now, id_owned],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Atomically delete every terminal row whose `finished_at` is
    /// strictly less than `cutoff`, returning each deleted row's
    /// `(id, workspace_path)` so the caller can purge the on-disk
    /// dirs. Active rows (Pending / Planning / Planned / Gathering /
    /// Synthesizing) and terminal rows with `finished_at >= cutoff`
    /// are preserved.
    ///
    /// The DB delete and the filesystem cleanup are decoupled
    /// intentionally: SQL atomicity guarantees the DB side; the
    /// caller does best-effort filesystem cleanup outside the
    /// transaction so a slow `remove_dir_all` can't keep the SQLite
    /// write-lock held.
    pub fn purge_terminal_older_than(
        &self,
        cutoff: i64,
    ) -> Result<Vec<(ResearchJobId, Option<String>)>, ResearchError> {
        // Two-phase: SELECT then DELETE in one transaction so a
        // concurrent insert can't change the working set between
        // queries. The window of "finished_at < cutoff AND status IN
        // (terminal)" is what defines the work; we re-key it to ids
        // for the DELETE so a row that flipped to terminal during
        // the SELECT (impossible today; defensive) doesn't get
        // accidentally swept.
        let rows = self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let mut stmt = tx.prepare(
                "SELECT id, workspace_path FROM state_research_jobs \
                 WHERE finished_at IS NOT NULL \
                   AND finished_at < ?1 \
                   AND status IN ('complete', 'failed', 'cancelled')",
            )?;
            let collected: Vec<(String, Option<String>)> = stmt
                .query_map(params![cutoff], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            for (id, _path) in &collected {
                tx.execute("DELETE FROM state_research_jobs WHERE id = ?1", params![id])?;
            }
            tx.commit()?;
            Ok(collected)
        })?;
        Ok(rows
            .into_iter()
            .map(|(id, path)| (ResearchJobId::from(id.as_str()), path))
            .collect())
    }

    /// Count rows in any of the active (non-terminal) statuses for
    /// the given conversation. Drives the chat-pane badge so the
    /// UI doesn't need to materialise + filter the full list.
    pub fn active_count_for_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<i64, ResearchError> {
        let cid = conversation_id.as_str().to_owned();
        let n: i64 = self.db.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM state_research_jobs \
                 WHERE conversation_id = ?1 AND status IN \
                   ('pending', 'planning', 'planned', 'gathering', 'synthesizing')",
                params![cid],
                |r| r.get(0),
            )?;
            Ok(n)
        })?;
        Ok(n)
    }

    /// Count active rows across the entire DB. Drives the
    /// `/api/admin/research/active_count` endpoint when no
    /// conversation scope is given. SQL COUNT instead of
    /// `list_all().filter()` so the operator dashboard's polling
    /// stays O(active) on the index, not O(history).
    pub fn active_count_global(&self) -> Result<i64, ResearchError> {
        let n: i64 = self.db.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM state_research_jobs \
                 WHERE status IN \
                   ('pending', 'planning', 'planned', 'gathering', 'synthesizing')",
                [],
                |r| r.get(0),
            )?;
            Ok(n)
        })?;
        Ok(n)
    }
}

fn row_to_research_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResearchJobRow> {
    let status_str: String = row.get(3)?;
    let status = ResearchJobStatus::parse(&status_str).unwrap_or(ResearchJobStatus::Failed);
    Ok(ResearchJobRow {
        id: ResearchJobId::from(row.get::<_, String>(0)?.as_str()),
        conversation_id: ConversationId::from(row.get::<_, String>(1)?.as_str()),
        query: row.get(2)?,
        status,
        caller_trust: row.get(4)?,
        card_id: row.get(5)?,
        plan_json: row.get(6)?,
        notes_json: row.get(7)?,
        workspace_path: row.get(8)?,
        attachment_id: row.get(9)?,
        error: row.get(10)?,
        overrides_json: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        started_at: row.get(14)?,
        finished_at: row.get(15)?,
    })
}

// -----------------------------------------------------------------
// ConfigStore
// -----------------------------------------------------------------

/// Operator-editable defaults for the research subsystem. One row per
/// DB; seeded by migration 0027 so reads on a fresh DB always succeed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchConfig {
    pub max_wall_clock_minutes: u32,
    pub max_total_tokens: u32,
    pub max_subqueries: u32,
    pub parallel_workers: u32,
    pub max_urls_per_subquery: u32,
    pub max_pages_total: u32,
    pub auto_cancel_after_idle_secs: u32,
    pub phase_gates: PhaseGates,
    /// `None` means "inherit from Settings → Search."
    pub default_search_provider: Option<String>,
    pub updated_at: i64,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            max_wall_clock_minutes: 30,
            max_total_tokens: 100_000,
            max_subqueries: 12,
            parallel_workers: 3,
            max_urls_per_subquery: 5,
            max_pages_total: 60,
            auto_cancel_after_idle_secs: 120,
            phase_gates: PhaseGates::PlanOnly,
            default_search_provider: None,
            updated_at: 0,
        }
    }
}

/// Patch type for `PUT /api/admin/settings/research`. Each field is
/// optional; `None` leaves the column untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResearchConfigUpdate {
    pub max_wall_clock_minutes: Option<u32>,
    pub max_total_tokens: Option<u32>,
    pub max_subqueries: Option<u32>,
    pub parallel_workers: Option<u32>,
    pub max_urls_per_subquery: Option<u32>,
    pub max_pages_total: Option<u32>,
    pub auto_cancel_after_idle_secs: Option<u32>,
    pub phase_gates: Option<PhaseGates>,
    /// Outer `Option` is "patch present?", inner `Option` is "set to
    /// NULL (inherit)?"
    pub default_search_provider: Option<Option<String>>,
}

pub struct ResearchConfigStore<'db> {
    db: &'db Database,
}

impl<'db> ResearchConfigStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Read the singleton row. Migration 0027 seeds it, so this
    /// returns the defaults on a fresh DB rather than `None`.
    pub fn get(&self) -> Result<ResearchConfig, ResearchError> {
        let row = self.db.with_conn(|c| {
            let got = c.query_row(
                "SELECT max_wall_clock_minutes, max_total_tokens, max_subqueries, \
                        parallel_workers, max_urls_per_subquery, max_pages_total, \
                        auto_cancel_after_idle_secs, phase_gates, \
                        default_search_provider, updated_at \
                 FROM config_research WHERE id = 1",
                [],
                |r| {
                    let phase_gates_str: String = r.get(7)?;
                    Ok(ResearchConfig {
                        max_wall_clock_minutes: r.get::<_, i64>(0)?.max(0) as u32,
                        max_total_tokens: r.get::<_, i64>(1)?.max(0) as u32,
                        max_subqueries: r.get::<_, i64>(2)?.max(0) as u32,
                        parallel_workers: r.get::<_, i64>(3)?.max(1) as u32,
                        max_urls_per_subquery: r.get::<_, i64>(4)?.max(0) as u32,
                        max_pages_total: r.get::<_, i64>(5)?.max(0) as u32,
                        auto_cancel_after_idle_secs: r.get::<_, i64>(6)?.max(0) as u32,
                        phase_gates: PhaseGates::parse(&phase_gates_str)
                            .unwrap_or(PhaseGates::PlanOnly),
                        default_search_provider: r.get(8)?,
                        updated_at: r.get(9)?,
                    })
                },
            )?;
            Ok(got)
        })?;
        Ok(row)
    }

    /// Apply a patch. Validates each numeric field's lower bound and
    /// the phase-gate vocabulary; rejects garbage with `Invalid` so
    /// the API layer can surface a 400.
    pub fn update(
        &self,
        patch: &ResearchConfigUpdate,
        now: i64,
    ) -> Result<ResearchConfig, ResearchError> {
        if let Some(v) = patch.max_wall_clock_minutes {
            if v == 0 || v > 24 * 60 {
                return Err(ResearchError::Invalid(format!(
                    "max_wall_clock_minutes must be in 1..=1440 (got {v})"
                )));
            }
        }
        if let Some(v) = patch.max_total_tokens {
            if v == 0 {
                return Err(ResearchError::Invalid(
                    "max_total_tokens must be positive".into(),
                ));
            }
        }
        if let Some(v) = patch.max_subqueries {
            if !(1..=64).contains(&v) {
                return Err(ResearchError::Invalid(format!(
                    "max_subqueries must be in 1..=64 (got {v})"
                )));
            }
        }
        if let Some(v) = patch.parallel_workers {
            if !(1..=16).contains(&v) {
                return Err(ResearchError::Invalid(format!(
                    "parallel_workers must be in 1..=16 (got {v})"
                )));
            }
        }
        if let Some(v) = patch.max_urls_per_subquery {
            if !(1..=20).contains(&v) {
                return Err(ResearchError::Invalid(format!(
                    "max_urls_per_subquery must be in 1..=20 (got {v})"
                )));
            }
        }
        if let Some(v) = patch.max_pages_total {
            if !(1..=500).contains(&v) {
                return Err(ResearchError::Invalid(format!(
                    "max_pages_total must be in 1..=500 (got {v})"
                )));
            }
        }
        if let Some(v) = patch.auto_cancel_after_idle_secs {
            if !(10..=3600).contains(&v) {
                return Err(ResearchError::Invalid(format!(
                    "auto_cancel_after_idle_secs must be in 10..=3600 (got {v})"
                )));
            }
        }
        // String columns can't go through the same SQLite null-vs-
        // value trick the patch types use elsewhere, so we materialise
        // the COALESCE-friendly "i64 numbers" up front and apply
        // each in one UPDATE.
        let prior = self.get()?;
        let max_wall = patch
            .max_wall_clock_minutes
            .unwrap_or(prior.max_wall_clock_minutes) as i64;
        let max_tok = patch.max_total_tokens.unwrap_or(prior.max_total_tokens) as i64;
        let max_sq = patch.max_subqueries.unwrap_or(prior.max_subqueries) as i64;
        let par_w = patch.parallel_workers.unwrap_or(prior.parallel_workers) as i64;
        let urls_sq = patch
            .max_urls_per_subquery
            .unwrap_or(prior.max_urls_per_subquery) as i64;
        let pages_total = patch.max_pages_total.unwrap_or(prior.max_pages_total) as i64;
        let idle = patch
            .auto_cancel_after_idle_secs
            .unwrap_or(prior.auto_cancel_after_idle_secs) as i64;
        let gates = patch
            .phase_gates
            .unwrap_or(prior.phase_gates)
            .as_str()
            .to_owned();
        let provider: Option<String> = match &patch.default_search_provider {
            Some(opt) => opt.clone(),
            None => prior.default_search_provider.clone(),
        };
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE config_research SET \
                    max_wall_clock_minutes = ?1, \
                    max_total_tokens = ?2, \
                    max_subqueries = ?3, \
                    parallel_workers = ?4, \
                    max_urls_per_subquery = ?5, \
                    max_pages_total = ?6, \
                    auto_cancel_after_idle_secs = ?7, \
                    phase_gates = ?8, \
                    default_search_provider = ?9, \
                    updated_at = ?10 \
                 WHERE id = 1",
                params![
                    max_wall,
                    max_tok,
                    max_sq,
                    par_w,
                    urls_sq,
                    pages_total,
                    idle,
                    gates,
                    provider,
                    now,
                ],
            )?;
            Ok(())
        })?;
        self.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::ids::{ConversationId, ResearchJobId};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn status_parse_round_trips_every_variant() {
        for s in [
            ResearchJobStatus::Pending,
            ResearchJobStatus::Planning,
            ResearchJobStatus::Planned,
            ResearchJobStatus::Gathering,
            ResearchJobStatus::Synthesizing,
            ResearchJobStatus::Complete,
            ResearchJobStatus::Failed,
            ResearchJobStatus::Cancelled,
        ] {
            assert_eq!(ResearchJobStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(ResearchJobStatus::parse("nonsense"), None);
    }

    #[test]
    fn terminal_only_for_terminal_states() {
        assert!(ResearchJobStatus::Complete.is_terminal());
        assert!(ResearchJobStatus::Failed.is_terminal());
        assert!(ResearchJobStatus::Cancelled.is_terminal());
        assert!(!ResearchJobStatus::Pending.is_terminal());
        assert!(!ResearchJobStatus::Planning.is_terminal());
        assert!(!ResearchJobStatus::Planned.is_terminal());
        assert!(!ResearchJobStatus::Gathering.is_terminal());
        assert!(!ResearchJobStatus::Synthesizing.is_terminal());
    }

    #[test]
    fn phase_gates_parse_round_trip() {
        for g in [
            PhaseGates::None,
            PhaseGates::PlanOnly,
            PhaseGates::EveryPhase,
        ] {
            assert_eq!(PhaseGates::parse(g.as_str()), Some(g));
        }
        assert_eq!(PhaseGates::default(), PhaseGates::PlanOnly);
    }

    #[test]
    fn insert_pending_round_trips() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        let row = store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "what's new in Kokoro 2026?",
                "Controller",
                None,
                100,
            )
            .unwrap();
        assert_eq!(row.status, ResearchJobStatus::Pending);
        assert_eq!(row.query, "what's new in Kokoro 2026?");
        assert_eq!(row.created_at, 100);
        assert_eq!(row.updated_at, 100);
        assert!(row.started_at.is_none());
        assert!(row.finished_at.is_none());
    }

    #[test]
    fn insert_pending_rejects_empty_query() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let err = store
            .insert_pending(
                &ResearchJobId::new(),
                &ConversationId::from("c"),
                "   ",
                "Controller",
                None,
                100,
            )
            .unwrap_err();
        assert!(matches!(err, ResearchError::Invalid(_)));
    }

    #[test]
    fn insert_pending_rejects_oversized_query() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let too_big = "x".repeat(8_001);
        let err = store
            .insert_pending(
                &ResearchJobId::new(),
                &ConversationId::from("c"),
                &too_big,
                "Controller",
                None,
                100,
            )
            .unwrap_err();
        assert!(matches!(err, ResearchError::Invalid(_)));
    }

    #[test]
    fn claim_next_pending_atomic_transitions_to_planning() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        let claimed = store.claim_next_pending("card-1", 200).unwrap().unwrap();
        assert_eq!(claimed.id.as_str(), id.as_str());
        assert_eq!(claimed.status, ResearchJobStatus::Planning);
        assert_eq!(claimed.card_id.as_deref(), Some("card-1"));
        assert_eq!(claimed.started_at, Some(200));
        // Second claim returns None.
        assert!(store.claim_next_pending("card-2", 300).unwrap().is_none());
    }

    #[test]
    fn claim_returns_pending_in_oldest_first_order() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let older = ResearchJobId::new();
        let newer = ResearchJobId::new();
        store
            .insert_pending(
                &older,
                &ConversationId::from("c"),
                "older",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store
            .insert_pending(
                &newer,
                &ConversationId::from("c"),
                "newer",
                "Controller",
                None,
                200,
            )
            .unwrap();
        let first = store.claim_next_pending("a", 250).unwrap().unwrap();
        assert_eq!(first.id.as_str(), older.as_str());
        let second = store.claim_next_pending("b", 260).unwrap().unwrap();
        assert_eq!(second.id.as_str(), newer.as_str());
    }

    #[test]
    fn set_planned_writes_blob_and_status() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("card-1", 150).unwrap();
        let plan = ResearchPlan {
            thesis: "test thesis".into(),
            steps: vec![PlanStep {
                query: "first sub".into(),
                rationale: Some("baseline".into()),
            }],
        };
        store.set_planned(&id, &plan, 200).unwrap();
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Planned);
        let summary = row.to_summary();
        assert_eq!(summary.plan.as_ref().unwrap().steps.len(), 1);
        assert_eq!(summary.plan.as_ref().unwrap().thesis, "test thesis");
    }

    #[test]
    fn finish_with_failed_records_error_and_finished_at() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("card-1", 150).unwrap();
        store
            .finish(&id, ResearchJobStatus::Failed, Some("boom"), None, 999)
            .unwrap();
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Failed);
        assert_eq!(row.error.as_deref(), Some("boom"));
        assert_eq!(row.finished_at, Some(999));
    }

    #[test]
    fn claim_next_pending_skips_already_running_rows() {
        // Adversarial: a row in `Planning` (just claimed by another
        // supervisor instance, or by a prior tick) must NOT be
        // re-claimed. The `WHERE status = 'pending'` predicate
        // inside the UPDATE is what enforces this; if it ever
        // regresses we'd silently double-claim and double-spawn
        // runners against the same job.
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        let claimed = store.claim_next_pending("card-a", 200).unwrap().unwrap();
        assert_eq!(claimed.status, ResearchJobStatus::Planning);
        // No more Pending rows exist; second claim returns None even
        // though the row still exists in another status.
        assert!(store.claim_next_pending("card-b", 300).unwrap().is_none());
    }

    #[test]
    fn set_planned_returns_not_found_for_unknown_id() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let plan = ResearchPlan {
            thesis: "t".into(),
            steps: vec![PlanStep {
                query: "q".into(),
                rationale: None,
            }],
        };
        let err = store
            .set_planned(&ResearchJobId::new(), &plan, 100)
            .unwrap_err();
        assert!(matches!(err, ResearchError::NotFound(_)));
    }

    #[test]
    fn finish_updates_updated_at_and_finished_at_in_lockstep() {
        // updated_at + finished_at must agree on the terminal
        // transition timestamp — the retention sweeper (C6) keys on
        // finished_at, the operator UI sorts by updated_at, and a
        // skew between them would create surprising "this just
        // updated, why is it being purged?" behaviour.
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("card-1", 150).unwrap();
        store
            .finish(&id, ResearchJobStatus::Complete, None, Some("att-1"), 999)
            .unwrap();
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.updated_at, 999);
        assert_eq!(row.finished_at, Some(999));
        assert_eq!(row.attachment_id.as_deref(), Some("att-1"));
    }

    #[test]
    fn finish_does_not_overwrite_already_terminal_row() {
        // Security-relevant invariant: a `finish(Complete)` after a
        // `cancel_active` MUST NOT silently resurrect the row.
        // Without this guard the runner's natural "synthesise
        // complete → finish(Complete)" path would undo an operator
        // cancel that fired mid-gather.
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        // Operator cancel lands first.
        assert!(store.cancel_active(&id, Some("operator"), 200).unwrap());
        let cancelled = store.get(&id).unwrap().unwrap();
        assert_eq!(cancelled.status, ResearchJobStatus::Cancelled);
        assert_eq!(cancelled.finished_at, Some(200));
        assert_eq!(cancelled.error.as_deref(), Some("operator"));
        // Runner's late "synthesise complete" finish call: must
        // be a no-op (returns false), must NOT overwrite the
        // Cancelled status, attachment_id, or error.
        let advanced = store
            .finish(
                &id,
                ResearchJobStatus::Complete,
                None,
                Some("late-attachment"),
                300,
            )
            .unwrap();
        assert!(!advanced, "finish on already-terminal row must be no-op");
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert_eq!(row.finished_at, Some(200), "finished_at must NOT advance");
        assert_eq!(
            row.error.as_deref(),
            Some("operator"),
            "operator's cancel reason must survive",
        );
        assert!(
            row.attachment_id.is_none(),
            "late attachment_id must NOT be stamped onto a cancelled row",
        );
    }

    #[test]
    fn set_planned_does_not_resurrect_a_cancelled_row() {
        // Adversarial scenario: operator cancels DURING the planner
        // LLM call. The runner's planner returns a fresh
        // ResearchPlan; without the status guard set_planned would
        // overwrite the Cancelled status with Planned, silently
        // resurrecting the job. The status guard makes this a
        // NotFound to the runner, which is the right shape — the
        // runner's phase loop treats NotFound as "row gone away,
        // exit cleanly."
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &ConversationId::from("c"), "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        // Cancel BEFORE the planner returns.
        assert!(store.cancel_active(&id, Some("operator"), 120).unwrap());
        // Runner's planner now lands and tries to set_planned. Must
        // surface as NotFound (not Ok).
        let plan = ResearchPlan {
            thesis: "t".into(),
            steps: vec![PlanStep {
                query: "q".into(),
                rationale: None,
            }],
        };
        let err = store.set_planned(&id, &plan, 200).unwrap_err();
        assert!(matches!(err, ResearchError::NotFound(_)));
        // Critical: row stays Cancelled, plan_json is NOT written.
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert!(row.plan_json.is_none());
    }

    #[test]
    fn set_notes_does_not_advance_updated_at_on_cancelled_row() {
        // Operator cancels mid-gather. A worker that already kicked
        // off its HTTP fan-out lands its persist_one_note write
        // afterward. Without the status guard the cancelled row's
        // updated_at would advance, surfacing the cancelled job at
        // the top of the operator's `/research` list (sorted by
        // updated_at).
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &ConversationId::from("c"), "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store.cancel_active(&id, Some("operator"), 200).unwrap();
        let row_before = store.get(&id).unwrap().unwrap();
        assert_eq!(row_before.updated_at, 200);
        let err = store
            .set_notes(
                &id,
                &[ResearchNote {
                    index: 0,
                    sub_query: "q".into(),
                    state: SubQueryState::Done,
                    excerpt: "stale".into(),
                    sources: vec![],
                    tokens_used: None,
                    error: None,
                }],
                300,
            )
            .unwrap_err();
        assert!(matches!(err, ResearchError::NotFound(_)));
        let row_after = store.get(&id).unwrap().unwrap();
        assert_eq!(
            row_after.updated_at, 200,
            "updated_at must NOT advance on a cancelled row",
        );
        assert!(
            row_after.notes_json.is_none(),
            "stale gather worker must not stamp notes_json on cancelled row",
        );
    }

    #[test]
    fn set_workspace_path_skips_cancelled_rows() {
        // Mirror the `set_notes` test: a runner that races a
        // cancel between claim and provision must not bump
        // updated_at.
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &ConversationId::from("c"), "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store.cancel_active(&id, Some("operator"), 200).unwrap();
        let err = store
            .set_workspace_path(&id, "/tmp/whatever", 300)
            .unwrap_err();
        assert!(matches!(err, ResearchError::NotFound(_)));
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.updated_at, 200);
        assert!(row.workspace_path.is_none());
    }

    #[test]
    fn active_count_global_counts_only_non_terminal_rows() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        // Three rows: two active (Pending + Gathering after
        // intermediate transitions), one terminal.
        let cid = ConversationId::from("c-global");
        let pending_id = ResearchJobId::new();
        let active_id = ResearchJobId::new();
        let done_id = ResearchJobId::new();
        store
            .insert_pending(&pending_id, &cid, "p", "Controller", None, 100)
            .unwrap();
        store
            .insert_pending(&active_id, &cid, "a", "Controller", None, 110)
            .unwrap();
        store
            .insert_pending(&done_id, &cid, "d", "Controller", None, 120)
            .unwrap();
        // Pull active_id forward into Gathering; pull done_id all
        // the way through to a terminal row. pending_id stays in
        // Pending.
        store.claim_next_pending("c1", 130).unwrap();
        store
            .set_planned(
                &pending_id,
                &ResearchPlan {
                    thesis: "t".into(),
                    steps: vec![PlanStep {
                        query: "q".into(),
                        rationale: None,
                    }],
                },
                131,
            )
            .ok();
        store.claim_next_pending("c2", 140).unwrap();
        store
            .finish(&active_id, ResearchJobStatus::Complete, None, Some("a"), 150)
            .ok();
        store.claim_next_pending("c3", 160).unwrap();
        store
            .finish(&done_id, ResearchJobStatus::Failed, Some("err"), None, 170)
            .ok();
        // Whatever order claim_next_pending picks up these rows in,
        // the active count should equal exactly the number of rows
        // that haven't been driven to a terminal status. With the
        // three transitions above, pending_id was advanced to
        // Planned (active), and the other two reached terminal.
        let active = store.active_count_global().unwrap();
        assert_eq!(active, 1);
    }

    #[test]
    fn active_count_global_zero_when_only_terminal_rows() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let cid = ConversationId::from("c-only-done");
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store
            .finish(&id, ResearchJobStatus::Complete, None, Some("att"), 200)
            .unwrap();
        assert_eq!(store.active_count_global().unwrap(), 0);
    }

    #[test]
    fn active_count_global_zero_on_empty_table() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        assert_eq!(store.active_count_global().unwrap(), 0);
    }

    #[test]
    fn finish_returns_true_on_normal_advancement() {
        // Round-trip the new bool return on the happy path.
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        let advanced = store
            .finish(&id, ResearchJobStatus::Complete, None, Some("att-1"), 200)
            .unwrap();
        assert!(advanced);
    }

    #[test]
    fn finish_with_non_terminal_status_errors() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        let err = store
            .finish(&id, ResearchJobStatus::Planning, None, None, 200)
            .unwrap_err();
        assert!(matches!(err, ResearchError::Invalid(_)));
    }

    #[test]
    fn cancel_active_flips_any_non_terminal_status() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        // From Pending.
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        assert!(store.cancel_active(&id, Some("operator"), 200).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Cancelled);
        assert_eq!(row.error.as_deref(), Some("operator"));
        assert_eq!(row.finished_at, Some(200));

        // From Planned.
        let id2 = ResearchJobId::new();
        store
            .insert_pending(
                &id2,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store
            .set_planned(
                &id2,
                &ResearchPlan {
                    thesis: "t".into(),
                    steps: vec![PlanStep {
                        query: "q".into(),
                        rationale: None,
                    }],
                },
                120,
            )
            .unwrap();
        assert!(store.cancel_active(&id2, None, 200).unwrap());
        assert_eq!(
            store.get(&id2).unwrap().unwrap().status,
            ResearchJobStatus::Cancelled,
        );
    }

    #[test]
    fn cancel_active_idempotent_on_terminal_rows() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store
            .finish(&id, ResearchJobStatus::Complete, None, Some("att"), 200)
            .unwrap();
        // Already terminal — cancel is a no-op (returns false, not
        // an error). Critical: must not flip Complete back to
        // Cancelled and lose the attachment_id.
        assert!(!store.cancel_active(&id, Some("late"), 300).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Complete);
        assert_eq!(row.attachment_id.as_deref(), Some("att"));
        assert_eq!(row.finished_at, Some(200));
    }

    #[test]
    fn list_for_conversation_filters_and_orders_newest_first() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        for (i, conv) in ["c1", "c2", "c1"].iter().enumerate() {
            store
                .insert_pending(
                    &ResearchJobId::new(),
                    &ConversationId::from(*conv),
                    &format!("q{i}"),
                    "Controller",
                    None,
                    100 + i as i64,
                )
                .unwrap();
        }
        let c1_rows = store
            .list_for_conversation(&ConversationId::from("c1"))
            .unwrap();
        assert_eq!(c1_rows.len(), 2);
        // Newest first.
        assert!(c1_rows[0].created_at >= c1_rows[1].created_at);
    }

    #[test]
    fn active_count_excludes_terminal_rows() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let cid = ConversationId::from("c");
        let active_id = ResearchJobId::new();
        let done_id = ResearchJobId::new();
        store
            .insert_pending(&active_id, &cid, "active", "Controller", None, 100)
            .unwrap();
        store
            .insert_pending(&done_id, &cid, "done", "Controller", None, 110)
            .unwrap();
        store
            .finish(
                &done_id,
                ResearchJobStatus::Complete,
                None,
                Some("att"),
                120,
            )
            .unwrap();
        assert_eq!(store.active_count_for_conversation(&cid).unwrap(), 1);
    }

    // -------------- gather-phase transitions --------------

    fn note(index: u32, query: &str, state: SubQueryState) -> ResearchNote {
        ResearchNote {
            index,
            sub_query: query.into(),
            state,
            excerpt: format!("excerpt for {query}"),
            sources: vec![ResearchSource {
                url: format!("https://example.com/{query}"),
                title: Some(query.to_owned()),
                fetched_ok: true,
                error: None,
            }],
            tokens_used: Some(123),
            error: None,
        }
    }

    #[test]
    fn mark_gathering_only_advances_planned_rows() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        // Pending → mark_gathering must NOT advance (status guard).
        assert!(!store.mark_gathering(&id, 200).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Pending);
        // Drive to Planned, THEN mark_gathering succeeds.
        store.claim_next_pending("card-1", 150).unwrap();
        store
            .set_planned(
                &id,
                &ResearchPlan {
                    thesis: "t".into(),
                    steps: vec![PlanStep {
                        query: "q1".into(),
                        rationale: None,
                    }],
                },
                160,
            )
            .unwrap();
        assert!(store.mark_gathering(&id, 200).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Gathering);
        assert_eq!(row.updated_at, 200);
        // Idempotency contract: calling again is a no-op (returns
        // false), not an error.
        assert!(!store.mark_gathering(&id, 300).unwrap());
    }

    #[test]
    fn set_notes_round_trips_into_summary() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        let notes = vec![
            note(0, "first", SubQueryState::Done),
            note(1, "second", SubQueryState::Running),
        ];
        store.set_notes(&id, &notes, 200).unwrap();
        let row = store.get(&id).unwrap().unwrap();
        let summary = row.to_summary();
        assert_eq!(summary.notes.len(), 2);
        assert_eq!(summary.notes[0].sub_query, "first");
        assert_eq!(summary.notes[0].state, SubQueryState::Done);
        assert_eq!(summary.notes[1].state, SubQueryState::Running);
    }

    #[test]
    fn set_notes_returns_not_found_for_unknown_id() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let err = store
            .set_notes(&ResearchJobId::new(), &[], 100)
            .unwrap_err();
        assert!(matches!(err, ResearchError::NotFound(_)));
    }

    #[test]
    fn mark_synthesizing_only_advances_gathering_rows() {
        let db = fresh_db();
        let store = ResearchJobStore::new(&db);
        let id = ResearchJobId::new();
        store
            .insert_pending(
                &id,
                &ConversationId::from("c"),
                "q",
                "Controller",
                None,
                100,
            )
            .unwrap();
        // From Pending — no advance.
        assert!(!store.mark_synthesizing(&id, 200).unwrap());
        store.claim_next_pending("card-1", 150).unwrap();
        store
            .set_planned(
                &id,
                &ResearchPlan {
                    thesis: "t".into(),
                    steps: vec![PlanStep {
                        query: "q1".into(),
                        rationale: None,
                    }],
                },
                160,
            )
            .unwrap();
        // From Planned — still no (must go through Gathering).
        assert!(!store.mark_synthesizing(&id, 250).unwrap());
        store.mark_gathering(&id, 300).unwrap();
        // Now from Gathering — succeeds.
        assert!(store.mark_synthesizing(&id, 350).unwrap());
        let row = store.get(&id).unwrap().unwrap();
        assert_eq!(row.status, ResearchJobStatus::Synthesizing);
    }

    #[test]
    fn sub_query_state_str_round_trips_each_variant() {
        for s in [
            SubQueryState::Pending,
            SubQueryState::Running,
            SubQueryState::Done,
            SubQueryState::Failed,
        ] {
            assert!(!s.as_str().is_empty());
        }
        // Distinct strings — guards a regression where two variants
        // collide on the same string and the SPA badge can't tell
        // them apart.
        let strs: std::collections::HashSet<_> = [
            SubQueryState::Pending,
            SubQueryState::Running,
            SubQueryState::Done,
            SubQueryState::Failed,
        ]
        .iter()
        .map(|s| s.as_str())
        .collect();
        assert_eq!(strs.len(), 4);
    }

    // -------------- ResearchConfigStore --------------

    #[test]
    fn config_get_returns_seeded_defaults_on_fresh_db() {
        let db = fresh_db();
        let cfg = ResearchConfigStore::new(&db).get().unwrap();
        assert_eq!(cfg.max_wall_clock_minutes, 30);
        assert_eq!(cfg.max_total_tokens, 100_000);
        assert_eq!(cfg.max_subqueries, 12);
        assert_eq!(cfg.parallel_workers, 3);
        assert_eq!(cfg.max_urls_per_subquery, 5);
        assert_eq!(cfg.max_pages_total, 60);
        assert_eq!(cfg.auto_cancel_after_idle_secs, 120);
        assert_eq!(cfg.phase_gates, PhaseGates::PlanOnly);
        assert!(cfg.default_search_provider.is_none());
    }

    #[test]
    fn config_update_round_trips_each_field() {
        let db = fresh_db();
        let store = ResearchConfigStore::new(&db);
        let saved = store
            .update(
                &ResearchConfigUpdate {
                    max_wall_clock_minutes: Some(60),
                    parallel_workers: Some(5),
                    phase_gates: Some(PhaseGates::None),
                    default_search_provider: Some(Some("brave".into())),
                    ..Default::default()
                },
                500,
            )
            .unwrap();
        assert_eq!(saved.max_wall_clock_minutes, 60);
        assert_eq!(saved.parallel_workers, 5);
        assert_eq!(saved.phase_gates, PhaseGates::None);
        assert_eq!(saved.default_search_provider.as_deref(), Some("brave"));
        assert_eq!(saved.updated_at, 500);
        // Untouched fields keep their seeded values.
        assert_eq!(saved.max_subqueries, 12);
    }

    #[test]
    fn config_update_clears_search_provider_with_inner_none() {
        let db = fresh_db();
        let store = ResearchConfigStore::new(&db);
        store
            .update(
                &ResearchConfigUpdate {
                    default_search_provider: Some(Some("brave".into())),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        let cleared = store
            .update(
                &ResearchConfigUpdate {
                    // Outer Some = "patch present", inner None = "set NULL"
                    default_search_provider: Some(None),
                    ..Default::default()
                },
                200,
            )
            .unwrap();
        assert!(cleared.default_search_provider.is_none());
    }

    #[test]
    fn config_update_rejects_garbage_numbers() {
        let db = fresh_db();
        let store = ResearchConfigStore::new(&db);
        let err = store
            .update(
                &ResearchConfigUpdate {
                    max_wall_clock_minutes: Some(0),
                    ..Default::default()
                },
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ResearchError::Invalid(_)));
        let err = store
            .update(
                &ResearchConfigUpdate {
                    parallel_workers: Some(0),
                    ..Default::default()
                },
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ResearchError::Invalid(_)));
        let err = store
            .update(
                &ResearchConfigUpdate {
                    max_subqueries: Some(1000),
                    ..Default::default()
                },
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ResearchError::Invalid(_)));
    }
}
