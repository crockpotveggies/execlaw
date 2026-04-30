//! On-disk workspace for one deep-research job.
//!
//! Each job gets `<root>/<job_id>/` containing:
//!
//!   * `plan.json` — pretty-printed planner output (human-readable so
//!     operators can grep it).
//!   * `notes/<n>.json` — per-sub-query notes (C4).
//!   * `report.md` — final synthesized report (C5).
//!
//! Filesystem (not encrypted DB) so reports stay greppable +
//! publishable. The `state_research_jobs` row carries the index;
//! the workspace holds bulky payloads.
//!
//! 2026-04-29.

use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::{ResearchNote, ResearchPlan};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding: {0}")]
    Encoding(String),
}

/// Operator-configurable workspace root. Defaults to
/// `~/.execlaw/research/`. The directory is created on first use.
#[derive(Debug, Clone)]
pub struct ResearchWorkspace {
    root: PathBuf,
}

impl ResearchWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default root: `<home>/.execlaw/research/` if home is
    /// resolvable; otherwise a process-local fallback under the
    /// current working directory so dev-server invocations without a
    /// resolvable home still work.
    pub fn default_root() -> PathBuf {
        match directories::UserDirs::new() {
            Some(d) => d.home_dir().join(".execlaw").join("research"),
            None => PathBuf::from(".execlaw").join("research"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the per-job dir + `notes/` subdir. Returns the absolute
    /// path. Idempotent — calling twice doesn't error.
    pub fn provision(&self, job_id: &ResearchJobId) -> Result<PathBuf, WorkspaceError> {
        let dir = self.root.join(job_id.as_str());
        std::fs::create_dir_all(dir.join("notes"))?;
        Ok(dir)
    }

    /// Write the planner output as pretty-printed JSON. The plan is
    /// also persisted in `state_research_jobs.plan_json` for the
    /// fast-path read; this file is the operator-grep view.
    pub fn write_plan(
        &self,
        job_id: &ResearchJobId,
        plan: &ResearchPlan,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        let path = dir.join("plan.json");
        let body = serde_json::to_string_pretty(plan)
            .map_err(|e| WorkspaceError::Encoding(e.to_string()))?;
        std::fs::write(&path, body)?;
        Ok(path)
    }

    /// Write one gather-phase note as `notes/<index>.json`. Pretty-
    /// printed so operators can grep it directly. Idempotent —
    /// re-writing the same index overwrites cleanly (a worker that
    /// retries lands here without duplicate files).
    pub fn write_note(
        &self,
        job_id: &ResearchJobId,
        note: &ResearchNote,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        let path = dir.join("notes").join(format!("{}.json", note.index));
        let body = serde_json::to_string_pretty(note)
            .map_err(|e| WorkspaceError::Encoding(e.to_string()))?;
        std::fs::write(&path, body)?;
        Ok(path)
    }

    /// Write the synthesized report to `report.md` (C5). Returns the
    /// absolute path; callers register that path with the
    /// `AttachmentStore` so transport plugins can `send_file` it on
    /// the CardClosed event.
    pub fn write_report(
        &self,
        job_id: &ResearchJobId,
        markdown: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        let path = dir.join("report.md");
        std::fs::write(&path, markdown)?;
        Ok(path)
    }

    /// Read the synthesized report back. Used by the
    /// `research_get_report` tool. Returns `Ok(None)` when the file
    /// hasn't been written yet (gather phase still running, or job
    /// failed before synthesize). Other I/O errors propagate.
    pub fn read_report(&self, job_id: &ResearchJobId) -> Result<Option<String>, WorkspaceError> {
        let path = self.root.join(job_id.as_str()).join("report.md");
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::Io(e)),
        }
    }

    /// Tear down a job's workspace. C6's retention sweeper calls this
    /// after the row's terminal `finished_at` ages past the global
    /// retention cutoff. Safe to call when the dir doesn't exist.
    pub fn purge(&self, job_id: &ResearchJobId) -> Result<(), WorkspaceError> {
        let dir = self.root.join(job_id.as_str());
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::research::PlanStep;

    #[test]
    fn provision_creates_job_and_notes_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let dir = ws.provision(&id).unwrap();
        assert!(dir.exists());
        assert!(dir.join("notes").exists());
    }

    #[test]
    fn provision_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let first = ws.provision(&id).unwrap();
        let second = ws.provision(&id).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn write_plan_round_trips_pretty_json() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let plan = ResearchPlan {
            thesis: "thesis".into(),
            steps: vec![PlanStep {
                query: "first".into(),
                rationale: Some("baseline".into()),
            }],
        };
        let path = ws.write_plan(&id, &plan).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"thesis\""));
        assert!(body.contains("\"first\""));
        // Newlines mean we got the pretty-printed form.
        assert!(body.contains('\n'));
    }

    #[test]
    fn write_note_lands_in_notes_subdir_and_overwrites_idempotently() {
        use execlaw_core::research::{ResearchNote, ResearchSource, SubQueryState};
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let note = ResearchNote {
            index: 3,
            sub_query: "anything".into(),
            state: SubQueryState::Done,
            excerpt: "v1".into(),
            sources: vec![ResearchSource {
                url: "https://example.com".into(),
                title: Some("ex".into()),
                fetched_ok: true,
                error: None,
            }],
            tokens_used: Some(42),
            error: None,
        };
        let path = ws.write_note(&id, &note).unwrap();
        assert!(path.ends_with("notes/3.json") || path.ends_with("notes\\3.json"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"v1\""));
        // Re-write with a different excerpt — must overwrite, not error.
        let note2 = ResearchNote {
            excerpt: "v2".into(),
            ..note
        };
        ws.write_note(&id, &note2).unwrap();
        let body2 = std::fs::read_to_string(&path).unwrap();
        assert!(body2.contains("\"v2\""));
        assert!(!body2.contains("\"v1\""));
    }

    #[test]
    fn purge_removes_workspace_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        ws.provision(&id).unwrap();
        ws.purge(&id).unwrap();
        assert!(!tmp.path().join(id.as_str()).exists());
    }

    #[test]
    fn purge_is_safe_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        // No provision call — dir doesn't exist. Purge must not error.
        ws.purge(&id).unwrap();
    }
}
