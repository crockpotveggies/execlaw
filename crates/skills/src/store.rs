//! [`SkillStore`] — pure-CRUD persistence layer for skills.
//!
//! All writes:
//!   * run [`crate::scanner::scan`] before touching the DB
//!   * apply size caps (body, resource, total)
//!   * happen inside a single transaction
//!   * emit a structured `tracing` event so the audit trail is
//!     immediately legible without depending on the (still-unwired)
//!     EventKind dispatch path. Phase B/C will wire the formal
//!     `SkillCreated` / `SkillVersionAdded` / `SkillPromoted` /
//!     `SkillArchived` / `SkillWriteRejected` event emission via the
//!     conversation event log once the integration shape is locked.
//!
//! The store does NOT enforce admin-only writes — that's the tool
//! layer's job (the write tools are simply not registered for non-
//! admin callers). This separation keeps the store ergonomic for the
//! plugin host (Phase B) and the auto-capture worker (Phase C), which
//! both need to write skills without going through the model-facing
//! tool surface.

use crate::model::{
    MAX_BODY_BYTES, MAX_RESOURCE_BYTES, MAX_SKILL_TOTAL_BYTES, NewProposal, NewSkill,
    NewSkillVersion, ProposalId, ProposalKind, ProposalState, RegistrationKind, ResourceBlob,
    ResourceBody, Skill, SkillError, SkillId, SkillIndexEntry, SkillMatch, SkillProposal,
    SkillResource, SkillState, SkillVersion, SkillView, VersionId, validate_skill_name,
};
use crate::scanner::{ScanInput, ScanVerdict, Strictness, scan};
use execlaw_core::db::{Database, DbError};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Pure-CRUD store. Cheap to clone (holds a `Database` handle, which
/// is itself an `Arc<Mutex<Connection>>`).
#[derive(Clone)]
pub struct SkillStore {
    db: Database,
}

impl SkillStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Borrow the underlying database handle. Useful for tests that
    /// want to peek at trigger-maintained state (refcount, FTS index).
    pub fn db(&self) -> &Database {
        &self.db
    }

    // ---------------------------------------------------------------
    // Read paths
    // ---------------------------------------------------------------

    /// Compact list for the `skills.list` tool. Returns one entry per
    /// non-archived skill, ordered by name (stable so prompt-cache
    /// prefixes don't churn).
    pub fn list_index(&self) -> Result<Vec<SkillIndexEntry>, SkillError> {
        self.db
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT s.name, v.description, s.state, v.version
                     FROM state_skills s
                     JOIN state_skill_versions v ON v.id = s.current_version_id
                     WHERE s.state != 'archived'
                     ORDER BY s.name",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        let name: String = r.get(0)?;
                        let description: String = r.get(1)?;
                        let state_s: String = r.get(2)?;
                        let version: u32 = r.get::<_, i64>(3)? as u32;
                        Ok((name, description, state_s, version))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })?
            .into_iter()
            .map(|(name, description, state_s, version)| {
                let state = SkillState::parse(&state_s).ok_or_else(|| {
                    SkillError::Db(DbError::Invariant(format!("unknown state: {state_s}")))
                })?;
                Ok(SkillIndexEntry {
                    name,
                    description,
                    state,
                    version,
                })
            })
            .collect()
    }

    /// Activation payload for the `skills.view` tool. Returns `None`
    /// if no such skill exists OR if the skill is archived (archived
    /// skills are invisible to the agent surface).
    pub fn view(&self, name: &str) -> Result<Option<SkillView>, SkillError> {
        let row: Option<(String, String, String, i64, String, String, i64)> =
            self.db.with_conn(|c| {
                Ok(c.query_row(
                    "SELECT s.name, v.description, s.state, v.version, v.body_md,
                            v.frontmatter_json, v.id
                     FROM state_skills s
                     JOIN state_skill_versions v ON v.id = s.current_version_id
                     WHERE s.name = ?1 AND s.state != 'archived'",
                    params![name],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    },
                )
                .optional()?)
            })?;

        let Some((name, description, state_s, version, body_md, frontmatter_json, version_id)) =
            row
        else {
            return Ok(None);
        };

        let state = SkillState::parse(&state_s).ok_or_else(|| {
            SkillError::Db(DbError::Invariant(format!("unknown state: {state_s}")))
        })?;

        let resource_paths: Vec<String> = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT path FROM state_skill_resources WHERE skill_version_id = ?1 ORDER BY path",
            )?;
            let rows = stmt
                .query_map(params![version_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        Ok(Some(SkillView {
            name,
            description,
            state,
            version: version as u32,
            body_md,
            frontmatter_json,
            resource_paths,
        }))
    }

    /// Read a bundled resource attached to a skill's current version.
    /// Returns `None` if the skill or resource is missing.
    pub fn resource(&self, name: &str, path: &str) -> Result<Option<SkillResource>, SkillError> {
        let row: Option<(String, i64, Vec<u8>)> = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT b.mime, b.size_bytes, b.bytes
                 FROM state_skills s
                 JOIN state_skill_versions v ON v.id = s.current_version_id
                 JOIN state_skill_resources r ON r.skill_version_id = v.id
                 JOIN state_blobs b ON b.sha256 = r.blob_sha
                 WHERE s.name = ?1 AND s.state != 'archived' AND r.path = ?2",
                params![name, path],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?)
        })?;

        let Some((mime, size_bytes, bytes)) = row else {
            return Ok(None);
        };

        let body = if mime_is_text(&mime) {
            match std::str::from_utf8(&bytes) {
                Ok(s) => ResourceBody::Text {
                    content: s.to_string(),
                },
                Err(_) => ResourceBody::Base64 {
                    content: base64_encode(&bytes),
                },
            }
        } else {
            ResourceBody::Base64 {
                content: base64_encode(&bytes),
            }
        };

        Ok(Some(SkillResource {
            path: path.to_string(),
            mime,
            size_bytes: size_bytes as u64,
            body,
        }))
    }

    /// FTS5-backed search. Returns top-K matches ranked by `bm25()`
    /// (lower rank = closer match). The query is sanitized to plain
    /// alphanumeric tokens (FTS5 implicit AND) so caller-supplied
    /// FTS5 operators (`AND` / `OR` / `NEAR` / `*` / `"`) are treated
    /// as literal terms rather than syntax. Trade-off: callers can't
    /// run advanced FTS5 queries through this surface; that's
    /// acceptable for the LLM-facing tool — power users go via the
    /// admin UI which can offer a separate raw-query mode (Phase D).
    pub fn search(&self, query: &str, k: u32) -> Result<Vec<SkillMatch>, SkillError> {
        let sanitized = sanitize_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let k = k.clamp(1, 50) as i64;
        let rows: Vec<(String, String, String, i64, f64)> = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT s.name, v.description, s.state, v.version, bm25(skill_search) AS rank
                 FROM skill_search
                 JOIN state_skill_versions v ON v.id = skill_search.rowid
                 JOIN state_skills s ON s.current_version_id = v.id
                 WHERE skill_search MATCH ?1 AND s.state != 'archived'
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![sanitized, k], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, f64>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        rows.into_iter()
            .map(|(name, description, state_s, version, rank)| {
                let state = SkillState::parse(&state_s).ok_or_else(|| {
                    SkillError::Db(DbError::Invariant(format!("unknown state: {state_s}")))
                })?;
                Ok(SkillMatch {
                    name,
                    description,
                    state,
                    version: version as u32,
                    rank,
                })
            })
            .collect()
    }

    /// Full row + current version metadata. Used by the admin UI.
    pub fn get(&self, name: &str) -> Result<Option<Skill>, SkillError> {
        let row: Option<(
            i64,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            i64,
            Option<i64>,
            i64,
        )> = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id, name, state, source, registration_kind, owning_plugin_id,
                        created_at, updated_at, archived_at, current_version_id
                 FROM state_skills WHERE name = ?1",
                params![name],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let Some((
            id,
            name,
            state_s,
            source,
            kind_s,
            owning_plugin_id,
            created_at,
            updated_at,
            archived_at,
            current_version_id,
        )) = row
        else {
            return Ok(None);
        };
        let state = SkillState::parse(&state_s).ok_or_else(|| {
            SkillError::Db(DbError::Invariant(format!("unknown state: {state_s}")))
        })?;
        let registration_kind = RegistrationKind::parse(&kind_s).ok_or_else(|| {
            SkillError::Db(DbError::Invariant(format!("unknown reg kind: {kind_s}")))
        })?;
        let current_version = self.read_version_by_id(VersionId(current_version_id))?;
        Ok(Some(Skill {
            id: SkillId(id),
            name,
            state,
            source,
            registration_kind,
            owning_plugin_id,
            current_version,
            created_at,
            updated_at,
            archived_at,
        }))
    }

    fn read_version_by_id(&self, id: VersionId) -> Result<SkillVersion, SkillError> {
        let row: (
            i64,
            i64,
            i64,
            String,
            String,
            String,
            String,
            String,
            i64,
            Option<String>,
            Option<i64>,
        ) = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id, skill_id, version, description, body_md, frontmatter_json,
                        body_sha256, authored_by, authored_at, promotion_notes, parent_version_id
                 FROM state_skill_versions WHERE id = ?1",
                params![id.0],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                },
            )?)
        })?;
        Ok(SkillVersion {
            id: VersionId(row.0),
            skill_id: SkillId(row.1),
            version: row.2 as u32,
            description: row.3,
            body_md: row.4,
            frontmatter_json: row.5,
            body_sha256: row.6,
            authored_by: row.7,
            authored_at: row.8,
            promotion_notes: row.9,
            parent_version_id: row.10.map(VersionId),
        })
    }

    // ---------------------------------------------------------------
    // Write paths — every one of these runs the scanner first.
    // ---------------------------------------------------------------

    /// Create a new skill in `trial` state with its initial version.
    pub fn create(
        &self,
        new: NewSkill,
        strictness: Strictness,
        now_ms: i64,
    ) -> Result<SkillId, SkillError> {
        validate_skill_name(&new.name)?;
        validate_frontmatter(&new.initial_version.frontmatter_json)?;
        validate_sizes(&new.initial_version.body_md, &new.resources)?;
        run_scanner(
            &new.initial_version.description,
            &new.initial_version.body_md,
            &new.initial_version.frontmatter_json,
            &new.resources,
            strictness,
            &new.name,
        )?;

        let version_sha = sha256_hex(new.initial_version.body_md.as_bytes());

        let result = self.db.transaction(|tx| {
            // Reject up front if the name is already taken.
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM state_skills WHERE name = ?1",
                    params![new.name],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Err(DbError::Invariant(format!(
                    "skill already exists: {}",
                    new.name
                )));
            }

            // Insert skill row first with a NULL current_version_id so we
            // can fill it in once we know the version's id.
            tx.execute(
                "INSERT INTO state_skills
                    (name, current_version_id, state, source, registration_kind,
                     owning_plugin_id, created_at, updated_at)
                 VALUES (?1, NULL, 'trial', ?2, ?3, ?4, ?5, ?5)",
                params![
                    new.name,
                    new.source,
                    new.registration_kind.as_str(),
                    new.owning_plugin_id,
                    now_ms,
                ],
            )?;
            let skill_id = tx.last_insert_rowid();

            tx.execute(
                "INSERT INTO state_skill_versions
                    (skill_id, version, description, body_md, frontmatter_json,
                     body_sha256, authored_by, authored_at, promotion_notes, parent_version_id)
                 VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                params![
                    skill_id,
                    new.initial_version.description,
                    new.initial_version.body_md,
                    new.initial_version.frontmatter_json,
                    version_sha,
                    new.initial_version.authored_by,
                    now_ms,
                    new.initial_version.promotion_notes,
                ],
            )?;
            let version_id = tx.last_insert_rowid();

            // Point the skill at its initial version.
            tx.execute(
                "UPDATE state_skills SET current_version_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![version_id, now_ms, skill_id],
            )?;

            // Attach resources.
            for r in &new.resources {
                upsert_blob(tx, &r.bytes, &r.mime, now_ms)?;
                let sha = sha256_hex(&r.bytes);
                tx.execute(
                    "INSERT INTO state_skill_resources(skill_version_id, path, blob_sha)
                     VALUES (?1, ?2, ?3)",
                    params![version_id, r.path, sha],
                )?;
            }

            Ok(skill_id)
        })?;

        tracing::info!(
            event = "skill.created",
            name = %new.name,
            source = %new.source,
            kind = %new.registration_kind.as_str(),
            "skill created"
        );
        Ok(SkillId(result))
    }

    /// Add a new version to an existing skill. Always advances
    /// `current_version_id` to the new version (callers wanting to
    /// hold a stable version while iterating a fork should write to a
    /// differently-named skill — Phase B).
    pub fn add_version(
        &self,
        name: &str,
        new_version: NewSkillVersion,
        strictness: Strictness,
        now_ms: i64,
    ) -> Result<VersionId, SkillError> {
        validate_frontmatter(&new_version.frontmatter_json)?;
        if (new_version.body_md.len() as u64) > MAX_BODY_BYTES {
            return Err(SkillError::BodyTooLarge {
                size: new_version.body_md.len() as u64,
                cap: MAX_BODY_BYTES,
            });
        }
        run_scanner(
            &new_version.description,
            &new_version.body_md,
            &new_version.frontmatter_json,
            &[],
            strictness,
            name,
        )?;

        let body_sha = sha256_hex(new_version.body_md.as_bytes());

        let result = self.db.transaction(|tx| {
            let row: Option<(i64, i64, i64, String)> = tx
                .query_row(
                    "SELECT id, current_version_id, COALESCE((
                         SELECT MAX(version) FROM state_skill_versions WHERE skill_id = state_skills.id
                     ), 0), state
                     FROM state_skills WHERE name = ?1",
                    params![name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((skill_id, current_version_id, max_version, state_s)) = row else {
                return Err(DbError::Invariant(format!("skill not found: {name}")));
            };
            if state_s == "archived" {
                return Err(DbError::Invariant(format!(
                    "cannot add version to archived skill: {name}"
                )));
            }

            tx.execute(
                "INSERT INTO state_skill_versions
                    (skill_id, version, description, body_md, frontmatter_json,
                     body_sha256, authored_by, authored_at, promotion_notes, parent_version_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    skill_id,
                    max_version + 1,
                    new_version.description,
                    new_version.body_md,
                    new_version.frontmatter_json,
                    body_sha,
                    new_version.authored_by,
                    now_ms,
                    new_version.promotion_notes,
                    current_version_id,
                ],
            )?;
            let version_id = tx.last_insert_rowid();

            tx.execute(
                "UPDATE state_skills SET current_version_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![version_id, now_ms, skill_id],
            )?;

            Ok(version_id)
        })?;

        tracing::info!(
            event = "skill.version_added",
            name = %name,
            "skill version added"
        );
        Ok(VersionId(result))
    }

    /// Promote a skill from `trial` → `stable`. Idempotent: promoting
    /// an already-stable skill is a no-op that returns Ok.
    pub fn promote(
        &self,
        name: &str,
        notes: Option<String>,
        now_ms: i64,
    ) -> Result<(), SkillError> {
        let promoted = self.db.transaction(|tx| {
            let state: Option<String> = tx
                .query_row(
                    "SELECT state FROM state_skills WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(state) = state else {
                return Err(DbError::Invariant(format!("skill not found: {name}")));
            };
            match state.as_str() {
                "stable" => Ok(false),
                "archived" => Err(DbError::Invariant(format!(
                    "cannot promote archived skill: {name}"
                ))),
                "trial" => {
                    tx.execute(
                        "UPDATE state_skills SET state = 'stable', updated_at = ?1 WHERE name = ?2",
                        params![now_ms, name],
                    )?;
                    if let Some(notes) = &notes {
                        tx.execute(
                            "UPDATE state_skill_versions
                             SET promotion_notes = ?1
                             WHERE id = (SELECT current_version_id FROM state_skills WHERE name = ?2)",
                            params![notes, name],
                        )?;
                    }
                    Ok(true)
                }
                other => Err(DbError::Invariant(format!("unknown state: {other}"))),
            }
        })?;

        if promoted {
            tracing::info!(event = "skill.promoted", name = %name, "skill promoted");
        }
        Ok(())
    }

    /// Archive a skill. Idempotent. Archived skills don't appear in
    /// `list_index` / `view` / `search` / `resource`.
    pub fn archive(&self, name: &str, now_ms: i64) -> Result<(), SkillError> {
        let archived = self.db.transaction(|tx| {
            let state: Option<String> = tx
                .query_row(
                    "SELECT state FROM state_skills WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(state) = state else {
                return Err(DbError::Invariant(format!("skill not found: {name}")));
            };
            if state == "archived" {
                return Ok(false);
            }
            tx.execute(
                "UPDATE state_skills SET state = 'archived', archived_at = ?1, updated_at = ?1
                 WHERE name = ?2",
                params![now_ms, name],
            )?;
            Ok(true)
        })?;
        if archived {
            tracing::info!(event = "skill.archived", name = %name, "skill archived");
        }
        Ok(())
    }

    /// Phase B — import or update a plugin-shipped skill.
    ///
    /// Behavior:
    ///   * If no skill with `name` exists → create as `Shipped`.
    ///   * If a `Shipped` skill with the same `owning_plugin_id`
    ///     already exists → add a new version (preserves history).
    ///   * If a skill with the same name exists but is owned by a
    ///     different plugin OR was admin-authored → return
    ///     `SkillError::AlreadyExists`. The caller (plugin install
    ///     flow) surfaces this to the operator; the plugin install
    ///     proceeds for other skills, and the operator can rename or
    ///     archive the conflicting skill before retrying.
    ///
    /// Always uses `Strict` scanner mode — plugin-shipped content
    /// must not introduce credentials.
    pub fn import_shipped(&self, new: NewSkill, now_ms: i64) -> Result<SkillId, SkillError> {
        validate_skill_name(&new.name)?;
        validate_frontmatter(&new.initial_version.frontmatter_json)?;
        validate_sizes(&new.initial_version.body_md, &new.resources)?;
        run_scanner(
            &new.initial_version.description,
            &new.initial_version.body_md,
            &new.initial_version.frontmatter_json,
            &new.resources,
            Strictness::Strict,
            &new.name,
        )?;

        // Owner check — Phase B only imports Shipped skills owned by
        // a plugin. Defensive guard so callers can't accidentally
        // route admin-authored skills through this method.
        let owning = match new.owning_plugin_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                return Err(SkillError::Db(execlaw_core::db::DbError::Invariant(
                    "import_shipped requires owning_plugin_id".into(),
                )));
            }
        };

        let body_sha = sha256_hex(new.initial_version.body_md.as_bytes());

        let id = self.db.transaction(|tx| {
            // Look up an existing row.
            let existing: Option<(i64, i64, String, String, Option<String>)> = tx
                .query_row(
                    "SELECT id, current_version_id, state, registration_kind, owning_plugin_id
                     FROM state_skills WHERE name = ?1",
                    params![new.name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;

            match existing {
                None => {
                    // Fresh insert. Honors the passed
                    // registration_kind so the same code path serves
                    // both ZIP-shipped (Shipped) and runtime-
                    // registered (Registered) skills.
                    tx.execute(
                        "INSERT INTO state_skills
                            (name, current_version_id, state, source, registration_kind,
                             owning_plugin_id, created_at, updated_at)
                         VALUES (?1, NULL, 'trial', ?2, ?3, ?4, ?5, ?5)",
                        params![
                            new.name,
                            new.source,
                            new.registration_kind.as_str(),
                            owning,
                            now_ms,
                        ],
                    )?;
                    let skill_id = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO state_skill_versions
                            (skill_id, version, description, body_md, frontmatter_json,
                             body_sha256, authored_by, authored_at, promotion_notes,
                             parent_version_id)
                         VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                        params![
                            skill_id,
                            new.initial_version.description,
                            new.initial_version.body_md,
                            new.initial_version.frontmatter_json,
                            body_sha,
                            new.initial_version.authored_by,
                            now_ms,
                            new.initial_version.promotion_notes,
                        ],
                    )?;
                    let version_id = tx.last_insert_rowid();
                    tx.execute(
                        "UPDATE state_skills SET current_version_id = ?1, updated_at = ?2
                         WHERE id = ?3",
                        params![version_id, now_ms, skill_id],
                    )?;
                    for r in &new.resources {
                        upsert_blob(tx, &r.bytes, &r.mime, now_ms)?;
                        let sha = sha256_hex(&r.bytes);
                        tx.execute(
                            "INSERT INTO state_skill_resources(skill_version_id, path, blob_sha)
                             VALUES (?1, ?2, ?3)",
                            params![version_id, r.path, sha],
                        )?;
                    }
                    Ok(skill_id)
                }
                Some((skill_id, current_version_id, state, kind, owning_id)) => {
                    // Conflict resolution. Both shipped + registered
                    // are plugin-owned so either can re-import as a
                    // new version of an existing plugin-owned skill.
                    if kind != "shipped" && kind != "registered" {
                        return Err(execlaw_core::db::DbError::Invariant(format!(
                            "name {} is owned by an admin- or agent-authored skill; \
                             plugin import would clobber it. Rename the plugin's skill \
                             or archive the existing one before retrying.",
                            new.name
                        )));
                    }
                    if owning_id.as_deref() != Some(&owning) {
                        return Err(execlaw_core::db::DbError::Invariant(format!(
                            "name {} is owned by plugin {:?}; refusing cross-plugin overwrite",
                            new.name, owning_id
                        )));
                    }
                    // Same plugin re-shipping — append a version.
                    let max_version: i64 = tx.query_row(
                        "SELECT COALESCE(MAX(version), 0) FROM state_skill_versions
                         WHERE skill_id = ?1",
                        params![skill_id],
                        |r| r.get(0),
                    )?;
                    tx.execute(
                        "INSERT INTO state_skill_versions
                            (skill_id, version, description, body_md, frontmatter_json,
                             body_sha256, authored_by, authored_at, promotion_notes,
                             parent_version_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            skill_id,
                            max_version + 1,
                            new.initial_version.description,
                            new.initial_version.body_md,
                            new.initial_version.frontmatter_json,
                            body_sha,
                            new.initial_version.authored_by,
                            now_ms,
                            new.initial_version.promotion_notes,
                            current_version_id,
                        ],
                    )?;
                    let version_id = tx.last_insert_rowid();
                    // Audit fix (2026-05-03): if the skill row was
                    // archived (e.g. plugin was previously
                    // uninstalled), reactivate it back to `trial`
                    // and clear `archived_at`. Without this the
                    // newly-imported version would be invisible to
                    // the agent because the row's state stays
                    // archived. The new version always becomes the
                    // current one.
                    if state == "archived" {
                        tx.execute(
                            "UPDATE state_skills
                             SET current_version_id = ?1, state = 'trial',
                                 archived_at = NULL, updated_at = ?2
                             WHERE id = ?3",
                            params![version_id, now_ms, skill_id],
                        )?;
                    } else {
                        tx.execute(
                            "UPDATE state_skills SET current_version_id = ?1, updated_at = ?2
                             WHERE id = ?3",
                            params![version_id, now_ms, skill_id],
                        )?;
                    }
                    // Re-attach resources to the NEW version. The old
                    // version's resources stay attached to it (history).
                    for r in &new.resources {
                        upsert_blob(tx, &r.bytes, &r.mime, now_ms)?;
                        let sha = sha256_hex(&r.bytes);
                        tx.execute(
                            "INSERT INTO state_skill_resources(skill_version_id, path, blob_sha)
                             VALUES (?1, ?2, ?3)",
                            params![version_id, r.path, sha],
                        )?;
                    }
                    Ok(skill_id)
                }
            }
        });

        match id {
            Ok(id) => {
                tracing::info!(
                    event = "skill.imported_from_plugin",
                    name = %new.name,
                    plugin = %owning,
                    "plugin-shipped skill imported"
                );
                Ok(SkillId(id))
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("name ") && msg.contains("owned by") {
                    Err(SkillError::AlreadyExists(new.name.clone()))
                } else {
                    Err(SkillError::Db(e))
                }
            }
        }
    }

    /// Phase B — archive every non-archived skill owned by a plugin.
    /// Called from the plugin uninstall path. Returns the archived
    /// skill names so the caller can log them.
    pub fn archive_for_plugin(
        &self,
        plugin_id: &str,
        now_ms: i64,
    ) -> Result<Vec<String>, SkillError> {
        let archived = self.db.transaction(|tx| {
            let names: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT name FROM state_skills
                     WHERE owning_plugin_id = ?1 AND state != 'archived'",
                )?;
                let rows = stmt
                    .query_map(params![plugin_id], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            tx.execute(
                "UPDATE state_skills
                 SET state = 'archived', archived_at = ?1, updated_at = ?1
                 WHERE owning_plugin_id = ?2 AND state != 'archived'",
                params![now_ms, plugin_id],
            )?;
            Ok(names)
        })?;
        if !archived.is_empty() {
            tracing::info!(
                event = "skill.plugin_uninstall_cascade",
                plugin = %plugin_id,
                count = archived.len(),
                "archived skills owned by uninstalled plugin"
            );
        }
        Ok(archived)
    }

    // ---------------------------------------------------------------
    // Phase D.1 — version history + proposal CRUD
    // ---------------------------------------------------------------

    /// List every version row for a skill, ordered by version
    /// ascending. Used by the admin UI's diff view.
    pub fn list_versions(&self, name: &str) -> Result<Vec<SkillVersion>, SkillError> {
        let rows: Vec<(
            i64,
            i64,
            i64,
            String,
            String,
            String,
            String,
            String,
            i64,
            Option<String>,
            Option<i64>,
        )> = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT v.id, v.skill_id, v.version, v.description, v.body_md,
                        v.frontmatter_json, v.body_sha256, v.authored_by, v.authored_at,
                        v.promotion_notes, v.parent_version_id
                 FROM state_skill_versions v
                 JOIN state_skills s ON s.id = v.skill_id
                 WHERE s.name = ?1
                 ORDER BY v.version ASC",
            )?;
            let rows = stmt
                .query_map(params![name], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .map(|row| SkillVersion {
                id: VersionId(row.0),
                skill_id: SkillId(row.1),
                version: row.2 as u32,
                description: row.3,
                body_md: row.4,
                frontmatter_json: row.5,
                body_sha256: row.6,
                authored_by: row.7,
                authored_at: row.8,
                promotion_notes: row.9,
                parent_version_id: row.10.map(VersionId),
            })
            .collect())
    }

    /// Persist an agent-generated proposal for operator review.
    /// Returns the new proposal id.
    pub fn submit_proposal(&self, new: NewProposal, now_ms: i64) -> Result<ProposalId, SkillError> {
        // Validate the name eagerly so a bad name doesn't sit in the
        // proposal table unreachable.
        validate_skill_name(&new.proposed_name)?;
        let target_id = new.target_skill_id.map(|s| s.0);
        let id = self.db.transaction(|tx| {
            // For version_fork: supersede any prior pending proposal
            // for the same target, so the operator only sees the
            // latest improvement suggestion per skill.
            if new.kind == ProposalKind::VersionFork {
                if let Some(t) = target_id {
                    tx.execute(
                        "UPDATE state_skill_proposals
                         SET state = 'superseded', reviewed_at = ?1
                         WHERE target_skill_id = ?2
                           AND state = 'pending'
                           AND proposal_kind = 'version_fork'",
                        params![now_ms, t],
                    )?;
                }
            }
            tx.execute(
                "INSERT INTO state_skill_proposals
                    (proposal_kind, target_skill_id, proposed_name, description, body_md,
                     frontmatter_json, source_run_id, trajectory_summary,
                     tool_calls_observed, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10)",
                params![
                    new.kind.as_str(),
                    target_id,
                    new.proposed_name,
                    new.description,
                    new.body_md,
                    new.frontmatter_json,
                    new.source_run_id,
                    new.trajectory_summary,
                    new.tool_calls_observed as i64,
                    now_ms,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })?;
        tracing::info!(
            event = "skill.proposal_submitted",
            kind = %new.kind.as_str(),
            name = %new.proposed_name,
            id,
            "skill proposal submitted for review"
        );
        Ok(ProposalId(id))
    }

    /// List proposals, optionally filtered by state. Newest first.
    pub fn list_proposals(
        &self,
        state: Option<ProposalState>,
    ) -> Result<Vec<SkillProposal>, SkillError> {
        let (sql, has_filter) = match state {
            Some(_) => (
                "SELECT id, proposal_kind, target_skill_id, proposed_name, description,
                        body_md, frontmatter_json, source_run_id, trajectory_summary,
                        tool_calls_observed, state, promoted_skill_id, promoted_version_id,
                        created_at, reviewed_at, reviewer, decision_notes
                 FROM state_skill_proposals
                 WHERE state = ?1
                 ORDER BY created_at DESC",
                true,
            ),
            None => (
                "SELECT id, proposal_kind, target_skill_id, proposed_name, description,
                        body_md, frontmatter_json, source_run_id, trajectory_summary,
                        tool_calls_observed, state, promoted_skill_id, promoted_version_id,
                        created_at, reviewed_at, reviewer, decision_notes
                 FROM state_skill_proposals
                 ORDER BY created_at DESC",
                false,
            ),
        };
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(sql)?;
            let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<SkillProposal> {
                let kind_s: String = r.get(1)?;
                let state_s: String = r.get(10)?;
                Ok(SkillProposal {
                    id: ProposalId(r.get(0)?),
                    kind: ProposalKind::parse(&kind_s).unwrap_or(ProposalKind::NewSkill),
                    target_skill_id: r.get::<_, Option<i64>>(2)?.map(SkillId),
                    proposed_name: r.get(3)?,
                    description: r.get(4)?,
                    body_md: r.get(5)?,
                    frontmatter_json: r.get(6)?,
                    source_run_id: r.get(7)?,
                    trajectory_summary: r.get(8)?,
                    tool_calls_observed: r.get::<_, i64>(9)?.max(0) as u32,
                    state: ProposalState::parse(&state_s).unwrap_or(ProposalState::Pending),
                    promoted_skill_id: r.get::<_, Option<i64>>(11)?.map(SkillId),
                    promoted_version_id: r.get::<_, Option<i64>>(12)?.map(VersionId),
                    created_at: r.get(13)?,
                    reviewed_at: r.get(14)?,
                    reviewer: r.get(15)?,
                    decision_notes: r.get(16)?,
                })
            };
            let rows: Vec<SkillProposal> = if has_filter {
                let s = state.unwrap().as_str();
                stmt.query_map(params![s], map)?
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                stmt.query_map([], map)?.collect::<Result<Vec<_>, _>>()?
            };
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Get one proposal by id. Returns `None` when no such row.
    pub fn get_proposal(&self, id: ProposalId) -> Result<Option<SkillProposal>, SkillError> {
        let mut all = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, proposal_kind, target_skill_id, proposed_name, description,
                        body_md, frontmatter_json, source_run_id, trajectory_summary,
                        tool_calls_observed, state, promoted_skill_id, promoted_version_id,
                        created_at, reviewed_at, reviewer, decision_notes
                 FROM state_skill_proposals WHERE id = ?1",
            )?;
            let rows: Vec<SkillProposal> = stmt
                .query_map(params![id.0], |r| {
                    let kind_s: String = r.get(1)?;
                    let state_s: String = r.get(10)?;
                    Ok(SkillProposal {
                        id: ProposalId(r.get(0)?),
                        kind: ProposalKind::parse(&kind_s).unwrap_or(ProposalKind::NewSkill),
                        target_skill_id: r.get::<_, Option<i64>>(2)?.map(SkillId),
                        proposed_name: r.get(3)?,
                        description: r.get(4)?,
                        body_md: r.get(5)?,
                        frontmatter_json: r.get(6)?,
                        source_run_id: r.get(7)?,
                        trajectory_summary: r.get(8)?,
                        tool_calls_observed: r.get::<_, i64>(9)?.max(0) as u32,
                        state: ProposalState::parse(&state_s).unwrap_or(ProposalState::Pending),
                        promoted_skill_id: r.get::<_, Option<i64>>(11)?.map(SkillId),
                        promoted_version_id: r.get::<_, Option<i64>>(12)?.map(VersionId),
                        created_at: r.get(13)?,
                        reviewed_at: r.get(14)?,
                        reviewer: r.get(15)?,
                        decision_notes: r.get(16)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(all.pop())
    }

    /// Approve a pending proposal. For `new_skill` proposals: creates
    /// a fresh skill via `create()`. For `version_fork` proposals:
    /// adds a new version to the target skill via `add_version()`.
    /// In either case, the proposal row is marked `approved` and
    /// linked to the resulting skill / version.
    pub fn approve_proposal(
        &self,
        id: ProposalId,
        reviewer: &str,
        notes: Option<String>,
        now_ms: i64,
    ) -> Result<SkillId, SkillError> {
        let p = self
            .get_proposal(id)?
            .ok_or_else(|| SkillError::NotFound(format!("proposal {}", id.0)))?;
        if p.state != ProposalState::Pending {
            return Err(SkillError::InvalidStateTransition {
                from: p.state.as_str().into(),
                to: "approved".into(),
            });
        }

        let skill_id = match p.kind {
            ProposalKind::NewSkill => {
                let new = NewSkill {
                    name: p.proposed_name.clone(),
                    source: format!("agent:{}", p.source_run_id),
                    registration_kind: RegistrationKind::Authored,
                    owning_plugin_id: None,
                    initial_version: NewSkillVersion {
                        description: p.description.clone(),
                        body_md: p.body_md.clone(),
                        frontmatter_json: p.frontmatter_json.clone(),
                        authored_by: format!("agent:{} (approved by {reviewer})", p.source_run_id),
                        promotion_notes: notes.clone(),
                    },
                    resources: vec![],
                };
                self.create(new, crate::scanner::Strictness::Strict, now_ms)?
            }
            ProposalKind::VersionFork => {
                let target = p.target_skill_id.ok_or_else(|| {
                    SkillError::Db(execlaw_core::db::DbError::Invariant(
                        "version_fork proposal missing target_skill_id".into(),
                    ))
                })?;
                let target_skill = self.db.with_conn(|c| {
                    let name: String = c.query_row(
                        "SELECT name FROM state_skills WHERE id = ?1",
                        params![target.0],
                        |r| r.get(0),
                    )?;
                    Ok(name)
                })?;
                let new_version = NewSkillVersion {
                    description: p.description.clone(),
                    body_md: p.body_md.clone(),
                    frontmatter_json: p.frontmatter_json.clone(),
                    authored_by: format!("agent:{} (approved by {reviewer})", p.source_run_id),
                    promotion_notes: notes.clone(),
                };
                self.add_version(
                    &target_skill,
                    new_version,
                    crate::scanner::Strictness::Strict,
                    now_ms,
                )?;
                target
            }
        };

        // Find the current_version_id for the (now-promoted) skill.
        let current_version_id: i64 = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT current_version_id FROM state_skills WHERE id = ?1",
                params![skill_id.0],
                |r| r.get(0),
            )?)
        })?;

        self.db.transaction(|tx| {
            tx.execute(
                "UPDATE state_skill_proposals
                 SET state = 'approved', promoted_skill_id = ?1, promoted_version_id = ?2,
                     reviewed_at = ?3, reviewer = ?4, decision_notes = ?5
                 WHERE id = ?6 AND state = 'pending'",
                params![
                    skill_id.0,
                    current_version_id,
                    now_ms,
                    reviewer,
                    notes,
                    id.0
                ],
            )?;
            Ok(())
        })?;

        tracing::info!(
            event = "skill.proposal_approved",
            id = id.0,
            reviewer,
            skill_id = skill_id.0,
            "skill proposal approved"
        );
        Ok(skill_id)
    }

    /// Reject a pending proposal. Idempotent — rejecting an already-
    /// rejected proposal is a no-op.
    pub fn reject_proposal(
        &self,
        id: ProposalId,
        reviewer: &str,
        notes: Option<String>,
        now_ms: i64,
    ) -> Result<(), SkillError> {
        self.db.transaction(|tx| {
            let cur_state: Option<String> = tx
                .query_row(
                    "SELECT state FROM state_skill_proposals WHERE id = ?1",
                    params![id.0],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(s) = cur_state else {
                return Err(execlaw_core::db::DbError::Invariant(format!(
                    "proposal {} not found",
                    id.0
                )));
            };
            if s == "approved" {
                return Err(execlaw_core::db::DbError::Invariant(format!(
                    "proposal {} already approved; cannot reject",
                    id.0
                )));
            }
            if s == "rejected" {
                return Ok(()); // idempotent
            }
            tx.execute(
                "UPDATE state_skill_proposals
                 SET state = 'rejected', reviewed_at = ?1, reviewer = ?2,
                     decision_notes = ?3
                 WHERE id = ?4",
                params![now_ms, reviewer, notes, id.0],
            )?;
            Ok(())
        })?;
        tracing::info!(
            event = "skill.proposal_rejected",
            id = id.0,
            reviewer,
            "skill proposal rejected"
        );
        Ok(())
    }

    /// List all (non-archived) skill names currently owned by a plugin.
    /// Used by the admin UI and by tests to verify import results.
    pub fn list_for_plugin(&self, plugin_id: &str) -> Result<Vec<String>, SkillError> {
        let names: Vec<String> = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT name FROM state_skills
                 WHERE owning_plugin_id = ?1 AND state != 'archived'
                 ORDER BY name",
            )?;
            let rows = stmt
                .query_map(params![plugin_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(names)
    }

    /// Phase D.3 — close every open `skill_invocations` row for the
    /// given conversation. Called by the chat handler at turn end so
    /// the reuse-update worker has accurate (invocation, outcome,
    /// tool_calls_made) tuples to evaluate.
    ///
    /// Returns `(invocation_id, skill_id)` for each row closed, so
    /// the caller can enqueue per-invocation reuse-update requests.
    pub fn close_open_invocations(
        &self,
        conversation_id: &str,
        outcome: &str,
        tool_calls_made: u32,
        now_ms: i64,
    ) -> Result<Vec<(i64, SkillId)>, SkillError> {
        if !matches!(outcome, "success" | "failure" | "aborted") {
            return Err(SkillError::Db(execlaw_core::db::DbError::Invariant(
                format!("invalid outcome string: {outcome}"),
            )));
        }
        let closures = self.db.transaction(|tx| {
            let pairs: Vec<(i64, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, skill_id FROM state_skill_invocations
                     WHERE conversation_id = ?1 AND outcome IS NULL",
                )?;
                stmt.query_map(params![conversation_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
            };
            tx.execute(
                "UPDATE state_skill_invocations
                 SET outcome = ?1, outcome_at = ?2, tool_calls_made = ?3
                 WHERE conversation_id = ?4 AND outcome IS NULL",
                params![outcome, now_ms, tool_calls_made as i64, conversation_id],
            )?;
            Ok(pairs)
        })?;
        Ok(closures.into_iter().map(|(i, s)| (i, SkillId(s))).collect())
    }

    /// Record a `skills.view` activation. Returns the invocation id
    /// for later `close_invocation` correlation.
    pub fn record_invocation(
        &self,
        skill_name: &str,
        conversation_id: &str,
        now_ms: i64,
    ) -> Result<i64, SkillError> {
        let id = self.db.transaction(|tx| {
            let row: (i64, i64) = tx.query_row(
                "SELECT s.id, s.current_version_id FROM state_skills s
                 WHERE s.name = ?1 AND s.state != 'archived'",
                params![skill_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            tx.execute(
                "INSERT INTO state_skill_invocations
                    (skill_id, skill_version_id, conversation_id, loaded_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![row.0, row.1, conversation_id, now_ms],
            )?;
            Ok(tx.last_insert_rowid())
        })?;
        tracing::info!(
            event = "skill.invoked",
            name = %skill_name,
            conversation_id = %conversation_id,
            "skill activated"
        );
        Ok(id)
    }
}

// -----------------------------------------------------------------
// Helpers (free functions, not methods, so they're easy to test in
// isolation and don't need a SkillStore handle).
// -----------------------------------------------------------------

fn run_scanner(
    description: &str,
    body_md: &str,
    frontmatter_json: &str,
    resources: &[ResourceBlob],
    strictness: Strictness,
    skill_name: &str,
) -> Result<(), SkillError> {
    let res_refs: Vec<(String, &[u8])> = resources
        .iter()
        .map(|r| (r.path.clone(), r.bytes.as_slice()))
        .collect();
    let input = ScanInput {
        body_md,
        description,
        frontmatter_json,
        resources: &res_refs,
    };
    match scan(&input, strictness) {
        ScanVerdict::Clean { warnings } => {
            // Warn-mode + warn-only findings: write proceeds, but we
            // still emit the diagnostic trail so the operator can see
            // what got through.
            if !warnings.is_empty() {
                let fields: Vec<String> = warnings.iter().map(|f| f.field.clone()).collect();
                tracing::warn!(
                    event = "skill.write_warning",
                    name = %skill_name,
                    warnings = warnings.len(),
                    ?fields,
                    "scanner found warn-severity items but write proceeded under Warn strictness"
                );
            }
            Ok(())
        }
        ScanVerdict::Suspicious { findings } => {
            let fields: Vec<String> = findings.iter().map(|f| f.field.clone()).collect();
            tracing::warn!(
                event = "skill.write_rejected",
                name = %skill_name,
                findings = findings.len(),
                ?fields,
                "secret scanner blocked skill write"
            );
            Err(SkillError::Blocked {
                findings: findings.len(),
                fields,
            })
        }
    }
}

fn validate_frontmatter(s: &str) -> Result<(), SkillError> {
    let _: serde_json::Value =
        serde_json::from_str(s).map_err(|e| SkillError::InvalidFrontmatter(e.to_string()))?;
    Ok(())
}

fn validate_sizes(body: &str, resources: &[ResourceBlob]) -> Result<(), SkillError> {
    if (body.len() as u64) > MAX_BODY_BYTES {
        return Err(SkillError::BodyTooLarge {
            size: body.len() as u64,
            cap: MAX_BODY_BYTES,
        });
    }
    let mut total = body.len() as u64;
    for r in resources {
        if (r.bytes.len() as u64) > MAX_RESOURCE_BYTES {
            return Err(SkillError::ResourceTooLarge {
                size: r.bytes.len() as u64,
                cap: MAX_RESOURCE_BYTES,
            });
        }
        total += r.bytes.len() as u64;
    }
    if total > MAX_SKILL_TOTAL_BYTES {
        return Err(SkillError::ResourceTooLarge {
            size: total,
            cap: MAX_SKILL_TOTAL_BYTES,
        });
    }
    Ok(())
}

fn upsert_blob(
    tx: &rusqlite::Transaction<'_>,
    bytes: &[u8],
    mime: &str,
    now_ms: i64,
) -> Result<(), DbError> {
    let sha = sha256_hex(bytes);
    tx.execute(
        "INSERT OR IGNORE INTO state_blobs(sha256, bytes, mime, size_bytes, refcount, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![sha, bytes, mime, bytes.len() as i64, now_ms],
    )?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn mime_is_text(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-sh"
                | "application/javascript"
        )
}

/// Reduce an arbitrary user query to a safe FTS5 expression. We split
/// on non-alphanumeric runs (Unicode-aware), drop empty pieces, and
/// re-join with spaces — FTS5's default tokenizer treats space-
/// separated bareword terms as an implicit AND, which is what a
/// typical caller actually wants. Operator characters (`AND`/`OR`/
/// `NEAR`/`*`/`"`) survive as literal tokens but the surrounding
/// whitespace ensures they're parsed as terms not syntax. Empty
/// after sanitization → empty result set, no SQL executed.
fn sanitize_fts_query(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn base64_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    // Tiny implementation — avoids pulling base64 as a direct dep
    // (still in workspace deps but core/skills doesn't need to grow
    // its dep set for this one path).
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        write!(
            &mut out,
            "{}{}{}{}",
            TABLE[((n >> 18) & 0x3f) as usize] as char,
            TABLE[((n >> 12) & 0x3f) as usize] as char,
            TABLE[((n >> 6) & 0x3f) as usize] as char,
            TABLE[(n & 0x3f) as usize] as char,
        )
        .unwrap();
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use pretty_assertions::assert_eq;

    fn fresh_store() -> SkillStore {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        SkillStore::new(db)
    }

    fn sample_new(name: &str, body: &str) -> NewSkill {
        NewSkill {
            name: name.to_string(),
            source: "admin:test".into(),
            registration_kind: RegistrationKind::Authored,
            owning_plugin_id: None,
            initial_version: NewSkillVersion {
                description: "test skill".into(),
                body_md: body.into(),
                frontmatter_json: r#"{"name":"test","tags":["test"]}"#.into(),
                authored_by: "admin:test".into(),
                promotion_notes: None,
            },
            resources: vec![],
        }
    }

    // --- create / read roundtrip ---

    #[test]
    fn create_and_view_roundtrip() {
        let s = fresh_store();
        s.create(
            sample_new("test/foo", "hello world"),
            Strictness::Strict,
            1000,
        )
        .unwrap();
        let v = s.view("test/foo").unwrap().unwrap();
        assert_eq!(v.name, "test/foo");
        assert_eq!(v.body_md, "hello world");
        assert_eq!(v.version, 1);
        assert_eq!(v.state, SkillState::Trial);
    }

    #[test]
    fn list_index_omits_archived() {
        let s = fresh_store();
        s.create(sample_new("a/one", "x"), Strictness::Strict, 1)
            .unwrap();
        s.create(sample_new("a/two", "y"), Strictness::Strict, 2)
            .unwrap();
        s.archive("a/one", 3).unwrap();
        let idx = s.list_index().unwrap();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].name, "a/two");
    }

    // --- name validation ---

    #[test]
    fn create_rejects_invalid_name() {
        let s = fresh_store();
        let bad = sample_new("BadName", "x");
        let err = s.create(bad, Strictness::Strict, 1).unwrap_err();
        assert!(matches!(err, SkillError::InvalidName(_)));
    }

    #[test]
    fn create_rejects_duplicate_name() {
        let s = fresh_store();
        s.create(sample_new("a/dup", "x"), Strictness::Strict, 1)
            .unwrap();
        let err = s
            .create(sample_new("a/dup", "y"), Strictness::Strict, 2)
            .unwrap_err();
        match err {
            SkillError::Db(DbError::Invariant(msg)) => assert!(msg.contains("already exists")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // --- versioning ---

    #[test]
    fn add_version_advances_current_pointer_and_records_parent() {
        let s = fresh_store();
        s.create(sample_new("a/v", "v1"), Strictness::Strict, 1)
            .unwrap();
        s.add_version(
            "a/v",
            NewSkillVersion {
                description: "v2 desc".into(),
                body_md: "v2 body".into(),
                frontmatter_json: "{}".into(),
                authored_by: "admin:test".into(),
                promotion_notes: None,
            },
            Strictness::Strict,
            2,
        )
        .unwrap();
        let v = s.view("a/v").unwrap().unwrap();
        assert_eq!(v.version, 2);
        assert_eq!(v.body_md, "v2 body");

        // Parent linkage is preserved.
        let full = s.get("a/v").unwrap().unwrap();
        assert!(full.current_version.parent_version_id.is_some());
    }

    #[test]
    fn version_numbers_are_monotonic_per_skill() {
        let s = fresh_store();
        s.create(sample_new("a/m", "1"), Strictness::Strict, 1)
            .unwrap();
        for i in 2..=5 {
            s.add_version(
                "a/m",
                NewSkillVersion {
                    description: format!("v{i}"),
                    body_md: format!("body{i}"),
                    frontmatter_json: "{}".into(),
                    authored_by: "admin:test".into(),
                    promotion_notes: None,
                },
                Strictness::Strict,
                i,
            )
            .unwrap();
        }
        let v = s.view("a/m").unwrap().unwrap();
        assert_eq!(v.version, 5);
    }

    // --- promote / archive ---

    #[test]
    fn promote_trial_to_stable_then_idempotent() {
        let s = fresh_store();
        s.create(sample_new("a/p", "x"), Strictness::Strict, 1)
            .unwrap();
        s.promote("a/p", Some("looks good".into()), 2).unwrap();
        let g = s.get("a/p").unwrap().unwrap();
        assert_eq!(g.state, SkillState::Stable);
        // Idempotent.
        s.promote("a/p", None, 3).unwrap();
        assert_eq!(s.get("a/p").unwrap().unwrap().state, SkillState::Stable);
    }

    #[test]
    fn cannot_promote_archived() {
        let s = fresh_store();
        s.create(sample_new("a/a", "x"), Strictness::Strict, 1)
            .unwrap();
        s.archive("a/a", 2).unwrap();
        let err = s.promote("a/a", None, 3).unwrap_err();
        assert!(matches!(err, SkillError::Db(DbError::Invariant(_))));
    }

    #[test]
    fn archived_skills_invisible_to_view_and_list_and_search() {
        let s = fresh_store();
        s.create(sample_new("a/inv", "x"), Strictness::Strict, 1)
            .unwrap();
        s.archive("a/inv", 2).unwrap();
        assert!(s.view("a/inv").unwrap().is_none());
        assert!(s.list_index().unwrap().iter().all(|e| e.name != "a/inv"));
    }

    // --- scanner integration ---

    #[test]
    fn scanner_blocks_create_with_credential_in_body() {
        let s = fresh_store();
        let bad = sample_new(
            "a/bad",
            "use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to call",
        );
        let err = s.create(bad, Strictness::Strict, 1).unwrap_err();
        match err {
            SkillError::Blocked { findings, .. } => assert!(findings >= 1),
            other => panic!("expected Blocked, got {other:?}"),
        }
        // And the row was NOT inserted.
        assert!(s.get("a/bad").unwrap().is_none());
    }

    #[test]
    fn scanner_lets_vault_reference_through() {
        let s = fresh_store();
        let mut n = sample_new("a/ok", "use {{vault:openai_api_key}} to call");
        n.initial_version.frontmatter_json = r#"{"api_key":"{{vault:my-key}}"}"#.into();
        s.create(n, Strictness::Strict, 1).unwrap();
        assert!(s.view("a/ok").unwrap().is_some());
    }

    // --- size caps ---

    #[test]
    fn body_too_large_is_rejected() {
        let s = fresh_store();
        let huge = "x".repeat((MAX_BODY_BYTES + 1) as usize);
        let n = sample_new("a/big", &huge);
        let err = s.create(n, Strictness::Strict, 1).unwrap_err();
        assert!(matches!(err, SkillError::BodyTooLarge { .. }));
    }

    #[test]
    fn resource_too_large_is_rejected() {
        let s = fresh_store();
        let mut n = sample_new("a/bigres", "x");
        n.resources.push(ResourceBlob {
            path: "big.bin".into(),
            mime: "application/octet-stream".into(),
            bytes: vec![0u8; (MAX_RESOURCE_BYTES + 1) as usize],
        });
        let err = s.create(n, Strictness::Strict, 1).unwrap_err();
        assert!(matches!(err, SkillError::ResourceTooLarge { .. }));
    }

    // --- resources + blob refcount ---

    #[test]
    fn resource_roundtrips_text_via_view_and_resource() {
        let s = fresh_store();
        let mut n = sample_new("a/res", "see scripts/run.py");
        n.resources.push(ResourceBlob {
            path: "scripts/run.py".into(),
            mime: "text/x-python".into(),
            bytes: b"#!/usr/bin/env python\nprint('ok')\n".to_vec(),
        });
        s.create(n, Strictness::Strict, 1).unwrap();
        let v = s.view("a/res").unwrap().unwrap();
        assert_eq!(v.resource_paths, vec!["scripts/run.py".to_string()]);
        let r = s.resource("a/res", "scripts/run.py").unwrap().unwrap();
        match r.body {
            ResourceBody::Text { content } => assert!(content.contains("print('ok')")),
            ResourceBody::Base64 { .. } => panic!("text mime must yield text body"),
        }
    }

    #[test]
    fn binary_resource_returns_base64() {
        let s = fresh_store();
        let mut n = sample_new("a/bin", "see image");
        n.resources.push(ResourceBlob {
            path: "logo.png".into(),
            mime: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        });
        s.create(n, Strictness::Strict, 1).unwrap();
        let r = s.resource("a/bin", "logo.png").unwrap().unwrap();
        match r.body {
            ResourceBody::Base64 { content } => {
                assert!(!content.is_empty());
                assert!(!content.contains('\0'));
            }
            ResourceBody::Text { .. } => panic!("binary mime must yield base64 body"),
        }
    }

    #[test]
    fn blob_refcount_reflects_attachments_and_dedups_identical_bytes() {
        let s = fresh_store();
        let bytes = b"shared content".to_vec();
        let mut n1 = sample_new("a/r1", "x");
        n1.resources.push(ResourceBlob {
            path: "a.txt".into(),
            mime: "text/plain".into(),
            bytes: bytes.clone(),
        });
        s.create(n1, Strictness::Strict, 1).unwrap();
        let mut n2 = sample_new("a/r2", "x");
        n2.resources.push(ResourceBlob {
            path: "b.txt".into(),
            mime: "text/plain".into(),
            bytes: bytes.clone(),
        });
        s.create(n2, Strictness::Strict, 2).unwrap();
        let sha = sha256_hex(&bytes);
        let (refcount, blob_count): (i64, i64) = s
            .db()
            .with_conn(|c| {
                let r: i64 = c.query_row(
                    "SELECT refcount FROM state_blobs WHERE sha256 = ?1",
                    params![sha],
                    |r| r.get(0),
                )?;
                let n: i64 = c.query_row(
                    "SELECT COUNT(*) FROM state_blobs WHERE sha256 = ?1",
                    params![sha],
                    |r| r.get(0),
                )?;
                Ok((r, n))
            })
            .unwrap();
        assert_eq!(blob_count, 1, "identical bytes must dedup to one blob");
        assert_eq!(refcount, 2, "two attachments must yield refcount 2");
    }

    #[test]
    fn archiving_skill_does_not_decrement_blob_refcount() {
        // We retain version history when a skill is archived, so
        // resources stay attached and blob refcounts don't drop.
        // GC is a separate pass that runs on full version deletion
        // (not in Phase A).
        let s = fresh_store();
        let mut n = sample_new("a/keep", "x");
        n.resources.push(ResourceBlob {
            path: "a.txt".into(),
            mime: "text/plain".into(),
            bytes: b"keep me".to_vec(),
        });
        s.create(n, Strictness::Strict, 1).unwrap();
        let sha = sha256_hex(b"keep me");
        s.archive("a/keep", 2).unwrap();
        let refcount: i64 = s
            .db()
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT refcount FROM state_blobs WHERE sha256 = ?1",
                    params![sha],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(refcount, 1, "archive must not GC blobs");
    }

    // --- search (FTS5) ---

    #[test]
    fn search_finds_skill_by_description_keyword() {
        let s = fresh_store();
        let mut n = sample_new("research/gather", "...");
        n.initial_version.description =
            "Use this when the user wants to gather research sources from the web".into();
        s.create(n, Strictness::Strict, 1).unwrap();
        let hits = s.search("research sources", 10).unwrap();
        assert!(!hits.is_empty(), "FTS must surface the seeded skill");
        assert_eq!(hits[0].name, "research/gather");
    }

    #[test]
    fn search_excludes_archived() {
        let s = fresh_store();
        let mut n = sample_new("a/findme", "...");
        n.initial_version.description = "discover me via search".into();
        s.create(n, Strictness::Strict, 1).unwrap();
        s.archive("a/findme", 2).unwrap();
        assert!(s.search("discover", 10).unwrap().is_empty());
    }

    // --- invocations ---

    #[test]
    fn record_invocation_writes_a_row() {
        let s = fresh_store();
        s.create(sample_new("a/inv", "x"), Strictness::Strict, 1)
            .unwrap();
        let id = s.record_invocation("a/inv", "conv-1", 100).unwrap();
        assert!(id > 0);
        let count: i64 = s
            .db()
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM state_skill_invocations WHERE conversation_id = ?1",
                    params!["conv-1"],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    // --- FTS5 query sanitization ---

    #[test]
    fn search_query_with_fts_operators_is_sanitized_not_errored() {
        // Audit fix: a query like `foo AND bar` or `foo "bar` must
        // not error or be interpreted as raw FTS5 syntax. After
        // sanitization, the underlying query is `foo AND bar` →
        // `foo AND bar` (no special chars), which FTS5 handles as
        // term-AND-term-AND-term.
        let s = fresh_store();
        let mut n = sample_new("research/x", "...");
        n.initial_version.description = "the foo and the bar".into();
        s.create(n, Strictness::Strict, 1).unwrap();
        // Each of these would error if passed raw to FTS5:
        let queries = [
            "foo \"bar",
            "foo AND bar",
            "foo OR bar",
            "foo NEAR bar",
            "foo*",
            "*foo",
            "foo & bar | baz",
            "(foo bar)",
            "'; DROP TABLE skills; --",
        ];
        for q in queries {
            let r = s.search(q, 5);
            assert!(r.is_ok(), "query {q:?} should not error: {:?}", r.err());
        }
    }

    #[test]
    fn search_empty_or_pure_punctuation_returns_no_rows() {
        let s = fresh_store();
        s.create(sample_new("a/x", "x"), Strictness::Strict, 1)
            .unwrap();
        assert!(s.search("", 5).unwrap().is_empty());
        assert!(s.search("    ", 5).unwrap().is_empty());
        assert!(s.search("!@#$%^&*()", 5).unwrap().is_empty());
    }

    #[test]
    fn sanitize_fts_query_unit() {
        assert_eq!(sanitize_fts_query("foo bar"), "foo bar");
        assert_eq!(sanitize_fts_query("foo \"AND\" bar"), "foo AND bar");
        assert_eq!(sanitize_fts_query(""), "");
        assert_eq!(sanitize_fts_query("...!!!"), "");
        assert_eq!(sanitize_fts_query("héllo wörld"), "héllo wörld");
    }

    #[test]
    fn record_invocation_for_archived_skill_errors() {
        let s = fresh_store();
        s.create(sample_new("a/g", "x"), Strictness::Strict, 1)
            .unwrap();
        s.archive("a/g", 2).unwrap();
        assert!(s.record_invocation("a/g", "conv-1", 3).is_err());
    }

    // --- adversarial ---

    #[test]
    fn write_with_secret_emits_no_db_row_at_all() {
        // The store must be all-or-nothing: a blocked write leaves
        // ZERO rows in the skill table, the version table, the blob
        // table, and the resource table.
        let s = fresh_store();
        let mut n = sample_new("a/sec", "ok");
        n.resources.push(ResourceBlob {
            path: "leak.env".into(),
            mime: "text/plain".into(),
            bytes: b"GITHUB_TOKEN=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890".to_vec(),
        });
        let err = s.create(n, Strictness::Strict, 1).unwrap_err();
        assert!(matches!(err, SkillError::Blocked { .. }));
        let counts: (i64, i64, i64, i64) = s
            .db()
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM state_skills),
                       (SELECT COUNT(*) FROM state_skill_versions),
                       (SELECT COUNT(*) FROM state_blobs),
                       (SELECT COUNT(*) FROM state_skill_resources)",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?)
            })
            .unwrap();
        assert_eq!(counts, (0, 0, 0, 0), "blocked write must leave zero rows");
    }

    // --- Phase D.1: list_versions ---

    #[test]
    fn list_versions_returns_versions_in_order() {
        let s = fresh_store();
        s.create(sample_new("a/v", "v1"), Strictness::Strict, 1)
            .unwrap();
        for i in 2..=4 {
            s.add_version(
                "a/v",
                NewSkillVersion {
                    description: format!("v{i}"),
                    body_md: format!("body{i}"),
                    frontmatter_json: "{}".into(),
                    authored_by: "test".into(),
                    promotion_notes: None,
                },
                Strictness::Strict,
                i,
            )
            .unwrap();
        }
        let versions = s.list_versions("a/v").unwrap();
        assert_eq!(versions.len(), 4);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[3].version, 4);
        assert_eq!(versions[3].body_md, "body4");
        // Parent linkage from v2 onward.
        assert!(versions[0].parent_version_id.is_none());
        assert!(versions[1].parent_version_id.is_some());
    }

    #[test]
    fn list_versions_for_unknown_skill_returns_empty() {
        let s = fresh_store();
        let v = s.list_versions("ghost/skill").unwrap();
        assert!(v.is_empty());
    }

    // --- Phase D.1: proposal CRUD ---

    fn sample_proposal(name: &str) -> NewProposal {
        NewProposal {
            kind: ProposalKind::NewSkill,
            target_skill_id: None,
            proposed_name: name.into(),
            description: "test description".into(),
            body_md: "test body".into(),
            frontmatter_json: "{}".into(),
            source_run_id: "run-1".into(),
            trajectory_summary: Some("3 tool calls".into()),
            tool_calls_observed: 3,
        }
    }

    #[test]
    fn submit_proposal_persists_pending_row() {
        let s = fresh_store();
        let id = s
            .submit_proposal(sample_proposal("agent/draft"), 100)
            .unwrap();
        let p = s.get_proposal(id).unwrap().unwrap();
        assert_eq!(p.proposed_name, "agent/draft");
        assert_eq!(p.state, ProposalState::Pending);
        assert!(p.promoted_skill_id.is_none());
    }

    #[test]
    fn submit_proposal_rejects_invalid_name() {
        let s = fresh_store();
        let mut p = sample_proposal("Invalid Name");
        p.proposed_name = "Bad Name".into();
        let err = s.submit_proposal(p, 1).unwrap_err();
        assert!(matches!(err, SkillError::InvalidName(_)));
    }

    #[test]
    fn list_proposals_filters_by_state() {
        let s = fresh_store();
        let p1 = s.submit_proposal(sample_proposal("a/p1"), 1).unwrap();
        let _p2 = s.submit_proposal(sample_proposal("a/p2"), 2).unwrap();
        s.reject_proposal(p1, "admin", None, 3).unwrap();
        let pending = s.list_proposals(Some(ProposalState::Pending)).unwrap();
        let rejected = s.list_proposals(Some(ProposalState::Rejected)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].proposed_name, "a/p1");
        assert_eq!(rejected[0].reviewer.as_deref(), Some("admin"));
    }

    #[test]
    fn approve_new_skill_proposal_creates_skill_and_links_promoted_ids() {
        let s = fresh_store();
        let id = s.submit_proposal(sample_proposal("agent/new"), 1).unwrap();
        let skill_id = s
            .approve_proposal(id, "admin", Some("looks good".into()), 100)
            .unwrap();
        // Skill exists.
        let g = s.get("agent/new").unwrap().unwrap();
        assert_eq!(g.id, skill_id);
        // Proposal links to it.
        let p = s.get_proposal(id).unwrap().unwrap();
        assert_eq!(p.state, ProposalState::Approved);
        assert_eq!(p.promoted_skill_id, Some(skill_id));
        assert!(p.promoted_version_id.is_some());
        assert_eq!(p.reviewer.as_deref(), Some("admin"));
    }

    #[test]
    fn approve_version_fork_proposal_adds_version_to_target_skill() {
        let s = fresh_store();
        // Pre-existing skill.
        let target_id = s
            .create(
                sample_new("research/sources", "v1 body"),
                Strictness::Strict,
                1,
            )
            .unwrap();
        // Fork proposal with new body.
        let mut p = sample_proposal("research/sources");
        p.kind = ProposalKind::VersionFork;
        p.target_skill_id = Some(target_id);
        p.body_md = "improved v2 body".into();
        let pid = s.submit_proposal(p, 2).unwrap();

        let returned_id = s.approve_proposal(pid, "admin", None, 3).unwrap();
        assert_eq!(returned_id, target_id);
        // Skill now has v2 with the proposal's body.
        let g = s.get("research/sources").unwrap().unwrap();
        assert_eq!(g.current_version.version, 2);
        assert_eq!(g.current_version.body_md, "improved v2 body");
        // Proposal closed + linked.
        let pp = s.get_proposal(pid).unwrap().unwrap();
        assert_eq!(pp.state, ProposalState::Approved);
    }

    #[test]
    fn reject_proposal_marks_state_and_records_reviewer() {
        let s = fresh_store();
        let id = s.submit_proposal(sample_proposal("a/x"), 1).unwrap();
        s.reject_proposal(id, "admin", Some("not generalizable".into()), 100)
            .unwrap();
        let p = s.get_proposal(id).unwrap().unwrap();
        assert_eq!(p.state, ProposalState::Rejected);
        assert_eq!(p.reviewer.as_deref(), Some("admin"));
        assert_eq!(p.decision_notes.as_deref(), Some("not generalizable"));
    }

    #[test]
    fn reject_already_rejected_is_idempotent() {
        let s = fresh_store();
        let id = s.submit_proposal(sample_proposal("a/x"), 1).unwrap();
        s.reject_proposal(id, "admin", None, 100).unwrap();
        s.reject_proposal(id, "admin2", None, 200).unwrap();
        let p = s.get_proposal(id).unwrap().unwrap();
        // First rejection sticks (the second was a no-op).
        assert_eq!(p.reviewer.as_deref(), Some("admin"));
    }

    #[test]
    fn approve_already_approved_returns_invalid_state_transition() {
        let s = fresh_store();
        let id = s.submit_proposal(sample_proposal("a/x"), 1).unwrap();
        s.approve_proposal(id, "admin", None, 100).unwrap();
        let err = s.approve_proposal(id, "admin", None, 200).unwrap_err();
        assert!(matches!(err, SkillError::InvalidStateTransition { .. }));
    }

    #[test]
    fn cannot_reject_already_approved_proposal() {
        let s = fresh_store();
        let id = s.submit_proposal(sample_proposal("a/x"), 1).unwrap();
        s.approve_proposal(id, "admin", None, 100).unwrap();
        let err = s.reject_proposal(id, "admin", None, 200).unwrap_err();
        assert!(matches!(err, SkillError::Db(_)));
    }

    #[test]
    fn submit_version_fork_supersedes_prior_pending_for_same_target() {
        let s = fresh_store();
        let target = s
            .create(sample_new("a/target", "x"), Strictness::Strict, 1)
            .unwrap();
        let mut p1 = sample_proposal("a/target");
        p1.kind = ProposalKind::VersionFork;
        p1.target_skill_id = Some(target);
        p1.body_md = "first improvement".into();
        let id1 = s.submit_proposal(p1.clone(), 100).unwrap();
        let mut p2 = p1;
        p2.body_md = "second improvement".into();
        let id2 = s.submit_proposal(p2, 200).unwrap();
        let pp1 = s.get_proposal(id1).unwrap().unwrap();
        let pp2 = s.get_proposal(id2).unwrap().unwrap();
        assert_eq!(pp1.state, ProposalState::Superseded);
        assert_eq!(pp2.state, ProposalState::Pending);
    }

    #[test]
    fn submit_new_skill_proposal_does_not_supersede_other_kinds() {
        // new_skill proposals don't have a target, so they don't
        // displace each other.
        let s = fresh_store();
        let id1 = s.submit_proposal(sample_proposal("a/p1"), 1).unwrap();
        let id2 = s.submit_proposal(sample_proposal("a/p2"), 2).unwrap();
        for id in [id1, id2] {
            let p = s.get_proposal(id).unwrap().unwrap();
            assert_eq!(p.state, ProposalState::Pending);
        }
    }

    #[test]
    fn malformed_frontmatter_is_rejected_before_db_write() {
        let s = fresh_store();
        let mut n = sample_new("a/fm", "ok");
        n.initial_version.frontmatter_json = "not json".into();
        let err = s.create(n, Strictness::Strict, 1).unwrap_err();
        assert!(matches!(err, SkillError::InvalidFrontmatter(_)));
    }
}
