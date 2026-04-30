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
use execlaw_core::research::ResearchPlan;
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
