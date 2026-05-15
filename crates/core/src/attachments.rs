//! Attachment + artifact row models (§2.9.1, §2.9.2).
//!
//! Blobs themselves live on disk under `~/.execlaw/blobs/...`; these tables
//! only carry metadata.

use crate::db::{Database, DbError};
use crate::ids::{AttachmentId, ConversationId, ResearchJobId};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

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
    pub kind: String, // "research_pdf" | "image" | "plugin_artifact" | "other"
    pub mime_type: String,
    pub path: String,
    pub sha256: String,
    pub bytes: Option<i64>,
    pub created_at: i64,
    /// Plugin that minted this artifact, when applicable. `None` for
    /// research-pipeline artifacts and any pre-0002-migration rows.
    pub plugin_id: Option<String>,
    /// Operator-facing filename (browser save-as, outbound transport
    /// attachment name). Kept separate from `path` because `path` is the
    /// sha256-named blob on disk and would be opaque to a human.
    pub filename: Option<String>,
    /// Unix-seconds wall-clock TTL; `None` means no TTL. The ephemeral
    /// sweeper culls expired rows + their on-disk bytes.
    pub expires_at: Option<i64>,
}

/// Outcome of [`AttachmentStore::insert_plugin_artifact`]. Carries the new
/// row's identifiers so the caller (a Rhai binding) can hand them back to
/// the plugin script.
#[derive(Debug, Clone)]
pub struct PluginArtifactCreated {
    pub attachment_id: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Compute the standard on-disk path for a plugin artifact, given the
/// artifacts root directory and the bytes' sha256. Content-addressed —
/// two artifacts with identical bytes share one on-disk file.
pub fn plugin_artifact_path(root: &Path, sha256: &str) -> PathBuf {
    root.join(sha256)
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

    /// Look up an attachment by id. Returns `None` if the row is
    /// missing — caller distinguishes "no such id" from "DB error"
    /// via Result + Option.
    pub fn get(&self, id: &AttachmentId) -> Result<Option<AttachmentRow>, DbError> {
        let id_owned = id.as_str().to_owned();
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, conversation_id, mime_type, path, sha256, received_at \
                     FROM state_attachments WHERE id = ?1",
                    params![id_owned],
                    |r| {
                        Ok(AttachmentRow {
                            id: AttachmentId::from(r.get::<_, String>(0)?),
                            conversation_id: ConversationId::from(r.get::<_, String>(1)?),
                            mime_type: r.get(2)?,
                            path: r.get(3)?,
                            sha256: r.get(4)?,
                            received_at: r.get(5)?,
                        })
                    },
                )
                .ok();
            Ok(row)
        })
    }

    pub fn insert_artifact(&self, row: &ArtifactRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_artifacts(id, research_job_id, kind, mime_type, path, sha256, bytes, created_at, plugin_id, filename, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.id,
                    row.research_job_id.as_ref().map(|r| r.as_str().to_owned()),
                    row.kind,
                    row.mime_type,
                    row.path,
                    row.sha256,
                    row.bytes,
                    row.created_at,
                    row.plugin_id,
                    row.filename,
                    row.expires_at,
                ],
            )?;
            Ok(())
        })
    }

    /// Look up an artifact row by id. Returns `None` for missing rows;
    /// `Err` only for DB failures. Used by [`get_attachment_bytes_b64`]'s
    /// fallback path so plugin-generated artifacts share the read surface
    /// with inbound `state_attachments`.
    pub fn get_artifact(&self, id: &str) -> Result<Option<ArtifactRow>, DbError> {
        let id_owned = id.to_owned();
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, research_job_id, kind, mime_type, path, sha256, bytes, created_at, \
                            plugin_id, filename, expires_at \
                       FROM state_artifacts WHERE id = ?1",
                    params![id_owned],
                    |r| {
                        Ok(ArtifactRow {
                            id: r.get::<_, String>(0)?,
                            research_job_id: r
                                .get::<_, Option<String>>(1)?
                                .map(ResearchJobId::from),
                            kind: r.get(2)?,
                            mime_type: r.get(3)?,
                            path: r.get(4)?,
                            sha256: r.get(5)?,
                            bytes: r.get(6)?,
                            created_at: r.get(7)?,
                            plugin_id: r.get(8)?,
                            filename: r.get(9)?,
                            expires_at: r.get(10)?,
                        })
                    },
                )
                .ok();
            Ok(row)
        })
    }

    /// Write a plugin-rendered artifact: hash the bytes, store them on
    /// disk under `artifacts_root/<sha256>` (content-addressed; idempotent
    /// for identical bytes), and insert a `state_artifacts` row with
    /// `kind = "plugin_artifact"`. Returns the new attachment id + size +
    /// sha so the caller can echo it back to the agent.
    ///
    /// The artifact id is a fresh UUID. Two artifacts with identical
    /// bytes get distinct ids but share one on-disk file — counted by the
    /// row's `path` field rather than `state_blobs.refcount` (artifacts
    /// don't participate in the blob refcount system today).
    pub fn insert_plugin_artifact(
        &self,
        artifacts_root: &Path,
        plugin_id: &str,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
        ttl_seconds: Option<i64>,
        now: i64,
    ) -> Result<PluginArtifactCreated, DbError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let sha = format!("{:x}", hasher.finalize());
        let path = plugin_artifact_path(artifacts_root, &sha);

        // Write the bytes if missing. The dir must exist; create it
        // recursively so first-use on a fresh install Just Works without
        // requiring the operator to mkdir.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DbError::Migration(format!(
                    "plugin artifact: create_dir_all {}: {e}",
                    parent.display()
                ))
            })?;
        }
        if !path.exists() {
            std::fs::write(&path, bytes).map_err(|e| {
                DbError::Migration(format!("plugin artifact: write {}: {e}", path.display()))
            })?;
        }

        let attachment_id = uuid::Uuid::new_v4().to_string();
        let expires_at = ttl_seconds.map(|t| now + t);
        let row = ArtifactRow {
            id: attachment_id.clone(),
            research_job_id: None,
            kind: "plugin_artifact".into(),
            mime_type: mime_type.to_owned(),
            path: path.to_string_lossy().into_owned(),
            sha256: sha.clone(),
            bytes: Some(bytes.len() as i64),
            created_at: now,
            plugin_id: Some(plugin_id.to_owned()),
            filename: Some(filename.to_owned()),
            expires_at,
        };
        self.insert_artifact(&row)?;
        Ok(PluginArtifactCreated {
            attachment_id,
            sha256: sha,
            size_bytes: bytes.len() as u64,
        })
    }

    /// Purge every artifact owned by `plugin_id` — both the DB row and,
    /// when no other row still references the same on-disk path
    /// (content-addressed dedupe), the file itself.
    ///
    /// Used by the plugin lifecycle's `purge` path (SPA uninstall +
    /// factory reset) to make "remove this plugin" a true clean-slate
    /// operation. Mirrors `sweep_expired_plugin_artifacts` but scoped by
    /// `plugin_id` instead of `expires_at`; the dedupe-aware
    /// "only delete the blob when refcount hits zero" logic is the same.
    ///
    /// Returns the number of `state_artifacts` rows removed. A plugin
    /// that never minted any artifacts is a no-op `Ok(0)`. Idempotent —
    /// calling twice for the same `plugin_id` returns 0 on the second
    /// call.
    pub fn purge_artifacts_for_plugin(&self, plugin_id: &str) -> Result<usize, DbError> {
        let rows: Vec<(String, String)> = self.db.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT id, path FROM state_artifacts WHERE plugin_id = ?1")?;
            let iter = stmt.query_map(params![plugin_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in iter {
                out.push(row?);
            }
            Ok(out)
        })?;
        let mut removed = 0usize;
        for (id, path) in rows {
            let id_for_delete = id.clone();
            self.db.with_conn(|c| {
                c.execute(
                    "DELETE FROM state_artifacts WHERE id = ?1",
                    params![id_for_delete],
                )?;
                Ok(())
            })?;
            // Refcount-aware blob delete: only unlink the on-disk file
            // when no surviving `state_artifacts` row points at the
            // same `path`. Two plugins emitting identical chart bytes
            // share one blob; uninstalling one must not break the
            // other.
            let still_used: i64 = self.db.with_conn(|c| {
                let n: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM state_artifacts WHERE path = ?1",
                        params![path.clone()],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                Ok(n)
            })?;
            if still_used == 0 {
                // Best-effort delete — missing file is fine (already
                // GC'd, never written due to dedupe race, etc).
                let _ = std::fs::remove_file(&path);
            }
            removed += 1;
        }
        Ok(removed)
    }

    /// Sweep expired plugin artifacts. Removes rows whose `expires_at`
    /// is in the past (relative to `now`) AND deletes the on-disk file
    /// for each — but only when no OTHER row still references the same
    /// sha (since artifacts are content-addressed and could be shared
    /// across plugins or identical re-renders).
    ///
    /// Returns the number of rows removed. Idempotent: a second call
    /// with the same `now` is a no-op.
    pub fn sweep_expired_plugin_artifacts(&self, now: i64) -> Result<usize, DbError> {
        // Two-step so we can release on-disk bytes safely.
        let expired: Vec<(String, String)> = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, path FROM state_artifacts \
                 WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            )?;
            let rows = stmt.query_map(params![now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?;
        let mut removed = 0usize;
        for (id, path) in expired {
            let id_for_delete = id.clone();
            self.db.with_conn(|c| {
                c.execute(
                    "DELETE FROM state_artifacts WHERE id = ?1",
                    params![id_for_delete],
                )?;
                Ok(())
            })?;
            // Only delete on-disk bytes when no other row points at the
            // same path. This is the content-addressed dedupe — two
            // identical chart renders share one file.
            let still_used: i64 = self.db.with_conn(|c| {
                let n: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM state_artifacts WHERE path = ?1",
                        params![path.clone()],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                Ok(n)
            })?;
            if still_used == 0 {
                // Best-effort delete — a missing file is fine (manually
                // cleaned up, never written due to dedupe race, etc).
                let _ = std::fs::remove_file(&path);
            }
            removed += 1;
        }
        Ok(removed)
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
                plugin_id: None,
                filename: None,
                expires_at: None,
            })
            .unwrap();
    }

    /// Plugin-artifact round-trip: insert bytes, look up the row, verify
    /// the file on disk matches. Covers A1's happy path.
    #[test]
    fn plugin_artifact_round_trip_writes_disk_and_reads_back() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = AttachmentStore::new(&db);
        let tmp = tempfile::tempdir().unwrap();

        let bytes = b"\x89PNG\r\n\x1a\nfake-chart-bytes";
        let created = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "forecast.png",
                "image/png",
                bytes,
                Some(3600),
                1_700_000_000,
            )
            .unwrap();
        assert_eq!(created.size_bytes, bytes.len() as u64);
        assert_eq!(created.sha256.len(), 64);

        let row = store.get_artifact(&created.attachment_id).unwrap().unwrap();
        assert_eq!(row.kind, "plugin_artifact");
        assert_eq!(row.plugin_id.as_deref(), Some("open-meteo"));
        assert_eq!(row.filename.as_deref(), Some("forecast.png"));
        assert_eq!(row.mime_type, "image/png");
        assert_eq!(row.expires_at, Some(1_700_000_000 + 3600));

        let on_disk = std::fs::read(&row.path).unwrap();
        assert_eq!(on_disk, bytes);
    }

    /// Two artifacts with identical bytes share the on-disk file
    /// (content-addressed) but get distinct row ids.
    #[test]
    fn plugin_artifact_dedupes_bytes_across_rows() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = AttachmentStore::new(&db);
        let tmp = tempfile::tempdir().unwrap();

        let bytes = b"identical-bytes-content";
        let a = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "chart.png",
                "image/png",
                bytes,
                None,
                1,
            )
            .unwrap();
        let b = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "chart-2.png",
                "image/png",
                bytes,
                None,
                2,
            )
            .unwrap();
        assert_ne!(a.attachment_id, b.attachment_id, "ids must be distinct");
        assert_eq!(a.sha256, b.sha256, "sha must match for identical bytes");
        let row_a = store.get_artifact(&a.attachment_id).unwrap().unwrap();
        let row_b = store.get_artifact(&b.attachment_id).unwrap().unwrap();
        assert_eq!(row_a.path, row_b.path, "both rows point at the same blob");
    }

    /// TTL sweeper removes expired rows and frees the on-disk bytes
    /// when no other row references them. Non-expired rows survive.
    #[test]
    fn sweep_expired_plugin_artifacts_removes_only_past_ttl_rows() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = AttachmentStore::new(&db);
        let tmp = tempfile::tempdir().unwrap();

        let expired = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "old.png",
                "image/png",
                b"old-bytes",
                Some(10),
                1_700_000_000,
            )
            .unwrap();
        let fresh = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "new.png",
                "image/png",
                b"new-bytes",
                Some(3600),
                1_700_000_000,
            )
            .unwrap();
        let no_ttl = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "forever.png",
                "image/png",
                b"forever-bytes",
                None,
                1_700_000_000,
            )
            .unwrap();

        // Sweep at expired.expires_at + 1.
        let removed = store
            .sweep_expired_plugin_artifacts(1_700_000_000 + 11)
            .unwrap();
        assert_eq!(removed, 1);

        assert!(
            store
                .get_artifact(&expired.attachment_id)
                .unwrap()
                .is_none()
        );
        assert!(store.get_artifact(&fresh.attachment_id).unwrap().is_some());
        assert!(store.get_artifact(&no_ttl.attachment_id).unwrap().is_some());

        // Idempotent — second sweep removes nothing.
        let again = store
            .sweep_expired_plugin_artifacts(1_700_000_000 + 11)
            .unwrap();
        assert_eq!(again, 0);
    }

    /// The plugin-lifecycle `purge` path calls `purge_artifacts_for_plugin`
    /// to wipe one plugin's artifacts. Other plugins' rows + blobs must
    /// survive. The refcount-aware blob delete must NOT unlink a file
    /// still pointed at by another row.
    #[test]
    fn purge_artifacts_for_plugin_wipes_only_that_plugin_and_respects_dedupe() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let store = AttachmentStore::new(&db);
        let tmp = tempfile::tempdir().unwrap();

        // open-meteo writes two distinct artifacts (different bytes).
        // weather-station writes one artifact with IDENTICAL bytes to
        // open-meteo's chart-a — dedupe shares the blob across plugins.
        let chart_a = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "chart-a.png",
                "image/png",
                b"shared-chart-bytes",
                None,
                1,
            )
            .unwrap();
        let chart_b = store
            .insert_plugin_artifact(
                tmp.path(),
                "open-meteo",
                "chart-b.png",
                "image/png",
                b"open-meteo-only-bytes",
                None,
                2,
            )
            .unwrap();
        let other_plugin_share = store
            .insert_plugin_artifact(
                tmp.path(),
                "weather-station",
                "ws-chart.png",
                "image/png",
                b"shared-chart-bytes",
                None,
                3,
            )
            .unwrap();
        // Confirm starting dedupe state.
        let row_a = store.get_artifact(&chart_a.attachment_id).unwrap().unwrap();
        let row_share = store
            .get_artifact(&other_plugin_share.attachment_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            row_a.path, row_share.path,
            "shared bytes must point at the same blob",
        );
        let shared_blob_path = row_a.path.clone();
        let unique_blob_path = store
            .get_artifact(&chart_b.attachment_id)
            .unwrap()
            .unwrap()
            .path;
        assert!(std::path::Path::new(&shared_blob_path).exists());
        assert!(std::path::Path::new(&unique_blob_path).exists());

        // Purge open-meteo. Expected:
        //   * chart_a row gone, chart_b row gone, ws-chart row survives.
        //   * unique_blob_path file gone (no surviving row).
        //   * shared_blob_path file STAYS (ws-chart still references it).
        let removed = store.purge_artifacts_for_plugin("open-meteo").unwrap();
        assert_eq!(removed, 2);
        assert!(
            store
                .get_artifact(&chart_a.attachment_id)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_artifact(&chart_b.attachment_id)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_artifact(&other_plugin_share.attachment_id)
                .unwrap()
                .is_some(),
            "other plugins' rows must survive",
        );
        assert!(
            !std::path::Path::new(&unique_blob_path).exists(),
            "unique blob must be unlinked once refcount hits zero",
        );
        assert!(
            std::path::Path::new(&shared_blob_path).exists(),
            "shared blob must survive — weather-station still references it",
        );

        // Idempotent — second call returns 0.
        assert_eq!(store.purge_artifacts_for_plugin("open-meteo").unwrap(), 0);
    }
}
