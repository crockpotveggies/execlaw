//! `research_jobs` row model (§2.9.1).
//!
//! The actual executor lives outside core; this module only persists job
//! metadata and progress.

use crate::db::{Database, DbError};
use crate::ids::{ConversationId, ResearchJobId};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResearchJobStatus {
    Queued,
    Executing,
    AwaitingFollowup,
    Succeeded,
    Failed,
    Cancelled,
}

impl ResearchJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResearchJobStatus::Queued => "queued",
            ResearchJobStatus::Executing => "executing",
            ResearchJobStatus::AwaitingFollowup => "awaiting_followup",
            ResearchJobStatus::Succeeded => "succeeded",
            ResearchJobStatus::Failed => "failed",
            ResearchJobStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchJobRow {
    pub id: ResearchJobId,
    pub conversation_id: ConversationId,
    pub prompt: String,
    pub status: ResearchJobStatus,
    pub progress_json: Option<Vec<u8>>,
    pub artifact_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

pub struct ResearchStore<'db> {
    db: &'db Database,
}

impl<'db> ResearchStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn insert(&self, row: &ResearchJobRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO research_jobs(id, conversation_id, prompt, status, progress_json, \
                                           artifact_id, created_at, updated_at, finished_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.id.as_str(),
                    row.conversation_id.as_str(),
                    row.prompt,
                    row.status.as_str(),
                    row.progress_json,
                    row.artifact_id,
                    row.created_at,
                    row.updated_at,
                    row.finished_at,
                ],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    #[test]
    fn research_insert_roundtrip() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = ResearchStore::new(&db);
        store
            .insert(&ResearchJobRow {
                id: ResearchJobId::new(),
                conversation_id: ConversationId::from("c"),
                prompt: "what's new in Kokoro 2026?".into(),
                status: ResearchJobStatus::Queued,
                progress_json: None,
                artifact_id: None,
                created_at: 1,
                updated_at: 1,
                finished_at: None,
            })
            .unwrap();
    }
}
