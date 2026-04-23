//! Attachment + artifact row models (§2.9.1, §2.9.2).
//!
//! Blobs themselves live on disk under `~/.execlaw/blobs/...`; these tables
//! only carry metadata.

use crate::db::{Database, DbError};
use crate::ids::{AttachmentId, ConversationId, ResearchJobId};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRow {
    pub id: AttachmentId,
    pub conversation_id: ConversationId,
    pub mime_type: String,
    pub path: String,
    pub sha256: String,
    pub received_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRow {
    pub id: String,
    pub research_job_id: Option<ResearchJobId>,
    pub kind: String, // "research_pdf" | "image" | "other"
    pub mime_type: String,
    pub path: String,
    pub sha256: String,
    pub bytes: Option<i64>,
    pub created_at: i64,
}

pub struct AttachmentStore<'db> {
    db: &'db Database,
}

impl<'db> AttachmentStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn insert(&self, row: &AttachmentRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_attachments(id, conversation_id, mime_type, path, sha256, received_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.id.as_str(),
                    row.conversation_id.as_str(),
                    row.mime_type,
                    row.path,
                    row.sha256,
                    row.received_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn insert_artifact(&self, row: &ArtifactRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_artifacts(id, research_job_id, kind, mime_type, path, sha256, bytes, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.id,
                    row.research_job_id.as_ref().map(|r| r.as_str().to_owned()),
                    row.kind,
                    row.mime_type,
                    row.path,
                    row.sha256,
                    row.bytes,
                    row.created_at,
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
    fn attachment_and_artifact_insert() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = AttachmentStore::new(&db);

        store
            .insert(&AttachmentRow {
                id: AttachmentId::new(),
                conversation_id: ConversationId::from("c"),
                mime_type: "image/jpeg".into(),
                path: "/tmp/x.jpg".into(),
                sha256: "abc".into(),
                received_at: 1,
            })
            .unwrap();

        store
            .insert_artifact(&ArtifactRow {
                id: "art1".into(),
                research_job_id: Some(ResearchJobId::from("rj-1")),
                kind: "research_pdf".into(),
                mime_type: "application/pdf".into(),
                path: "/tmp/r.pdf".into(),
                sha256: "def".into(),
                bytes: Some(12345),
                created_at: 2,
            })
            .unwrap();
    }
}
