//! Phase B — plugin-shipped skill import.
//!
//! Bridges `execlaw_plugin_sdk::SkillDecl` (from a plugin's
//! `plugin.toml` manifest) to the [`crate::store::SkillStore`]'s
//! `import_shipped` write path.
//!
//! Responsibilities:
//!
//! 1. **Name sanitization + namespacing.** The plugin author's local
//!    `name` is sanitized (lowercase, alphanumeric + hyphen only,
//!    slashes stripped) and prepended with `<plugin_id>/`. The
//!    plugin author cannot author a skill outside their own
//!    namespace.
//! 2. **Body load.** `entry` is resolved against the plugin's staged
//!    root and read as UTF-8 markdown. Path traversal (`..`,
//!    absolute paths) is rejected.
//! 3. **Frontmatter construction.** A structured JSON object is
//!    derived from the manifest (`{name, description, tags}`) so
//!    the admin UI and analytics have a stable shape regardless of
//!    what's inside the body.
//! 4. **Import.** Each declared skill is sent through
//!    `SkillStore::import_shipped`, which scanner-gates the write
//!    and either creates a fresh row or appends a new version.
//!
//! Returns an [`ImportReport`] with per-skill outcomes so the plugin
//! install flow can surface a partial-success summary to the operator.

use crate::model::{NewSkill, NewSkillVersion, RegistrationKind, SkillError};
use crate::store::SkillStore;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Outcome of importing every skill declared by a plugin.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// Skills that landed cleanly (full namespaced name).
    pub imported: Vec<String>,
    /// Skills that failed to import + the reason.
    pub failed: Vec<ImportFailure>,
}

#[derive(Debug)]
pub struct ImportFailure {
    pub plugin_local_name: String,
    pub stored_name: Option<String>,
    pub error: SkillError,
}

/// Sanitize a plugin author's local skill name to the segment shape
/// allowed inside `<plugin_id>/<segment>`. Lowercase the input,
/// replace slashes with hyphens, drop any character that isn't
/// `[a-z0-9-]`, collapse runs of `-`, and trim leading/trailing `-`.
pub fn sanitize_local_name(name: &str) -> String {
    let lowered: String = name.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut out = String::with_capacity(lowered.len());
    let mut last_dash = true; // also collapses leading dashes
    for ch in lowered.chars() {
        let mapped = if ch == '/' {
            '-'
        } else if ch.is_ascii_alphanumeric() || ch == '-' {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(mapped);
            last_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Build the canonical stored name for a plugin-shipped skill. Always
/// `<plugin_id>/<sanitized_local_name>`; the plugin author cannot
/// influence the namespace.
pub fn namespaced_name(plugin_id: &str, local: &str) -> String {
    format!("{}/{}", plugin_id, sanitize_local_name(local))
}

/// Reject path traversal and absolute paths — the entry must live
/// inside the staged plugin root.
fn safe_resolve(stage_dir: &Path, entry: &str) -> Result<PathBuf, SkillError> {
    let entry_path = Path::new(entry);
    if entry_path.is_absolute() {
        return Err(SkillError::Db(execlaw_core::db::DbError::Invariant(
            format!("skill entry must be relative: {entry}"),
        )));
    }
    for component in entry_path.components() {
        match component {
            std::path::Component::ParentDir | std::path::Component::RootDir => {
                return Err(SkillError::Db(execlaw_core::db::DbError::Invariant(
                    format!("skill entry path traversal rejected: {entry}"),
                )));
            }
            _ => {}
        }
    }
    Ok(stage_dir.join(entry_path))
}

/// Manifest-backed import. `decls` come from
/// `PluginManifest::skills`; `stage_dir` is the unpacked plugin's
/// root directory; `now_ms` is the wall-clock timestamp the host
/// would have written into `state_plugins.updated_at`.
pub fn import_plugin_skills(
    store: &SkillStore,
    plugin_id: &str,
    decls: &[execlaw_plugin_sdk::manifest::SkillDecl],
    stage_dir: &Path,
    now_ms: i64,
) -> ImportReport {
    let mut report = ImportReport::default();
    for decl in decls {
        let stored_name = namespaced_name(plugin_id, &decl.name);
        match try_import_one(store, plugin_id, decl, stage_dir, &stored_name, now_ms) {
            Ok(()) => report.imported.push(stored_name),
            Err(error) => report.failed.push(ImportFailure {
                plugin_local_name: decl.name.clone(),
                stored_name: Some(stored_name),
                error,
            }),
        }
    }
    report
}

fn try_import_one(
    store: &SkillStore,
    plugin_id: &str,
    decl: &execlaw_plugin_sdk::manifest::SkillDecl,
    stage_dir: &Path,
    stored_name: &str,
    now_ms: i64,
) -> Result<(), SkillError> {
    let body_path = safe_resolve(stage_dir, &decl.entry)?;
    let body = std::fs::read_to_string(&body_path).map_err(|e| {
        SkillError::Db(execlaw_core::db::DbError::Io(std::io::Error::new(
            e.kind(),
            format!("reading skill entry {}: {e}", body_path.display()),
        )))
    })?;
    let frontmatter = json!({
        "name": stored_name,
        "description": decl.description,
        "tags": decl.tags,
    })
    .to_string();
    let new = NewSkill {
        name: stored_name.to_string(),
        source: format!("plugin:{plugin_id}"),
        registration_kind: RegistrationKind::Shipped,
        owning_plugin_id: Some(plugin_id.to_string()),
        initial_version: NewSkillVersion {
            description: decl.description.clone(),
            body_md: body,
            frontmatter_json: frontmatter,
            authored_by: format!("plugin:{plugin_id}"),
            promotion_notes: None,
        },
        resources: vec![],
    };
    store.import_shipped(new, now_ms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SkillState;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_plugin_sdk::manifest::SkillDecl;
    use tempfile::tempdir;

    fn fresh_store() -> SkillStore {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        SkillStore::new(db)
    }

    // --- sanitization ---

    #[test]
    fn sanitize_lowercases_and_strips_punctuation() {
        assert_eq!(sanitize_local_name("Query Builder"), "query-builder");
        assert_eq!(sanitize_local_name("Query/Builder"), "query-builder");
        assert_eq!(sanitize_local_name("query!!builder"), "query-builder");
        assert_eq!(sanitize_local_name("--leading-dashes--"), "leading-dashes");
        assert_eq!(
            sanitize_local_name("MULTI___under___score"),
            "multi-under-score"
        );
    }

    #[test]
    fn namespaced_name_always_uses_plugin_id_namespace() {
        // Even if the plugin author tries to escape into another
        // namespace, sanitization collapses the slash to a hyphen
        // and the prefix lock takes effect.
        assert_eq!(
            namespaced_name("postgres-toolkit", "query-builder"),
            "postgres-toolkit/query-builder"
        );
        assert_eq!(
            namespaced_name("postgres-toolkit", "core/admin"),
            "postgres-toolkit/core-admin"
        );
        assert_eq!(
            namespaced_name("postgres-toolkit", "/escape/"),
            "postgres-toolkit/escape"
        );
    }

    // --- path traversal defense ---

    #[test]
    fn safe_resolve_rejects_parent_dir() {
        let dir = tempdir().unwrap();
        let r = safe_resolve(dir.path(), "../etc/passwd");
        assert!(r.is_err());
    }

    #[test]
    fn safe_resolve_rejects_absolute() {
        let dir = tempdir().unwrap();
        // Use a platform-appropriate absolute path.
        let abs = if cfg!(windows) {
            "C:\\foo"
        } else {
            "/etc/passwd"
        };
        let r = safe_resolve(dir.path(), abs);
        assert!(r.is_err());
    }

    #[test]
    fn safe_resolve_accepts_nested_relative() {
        let dir = tempdir().unwrap();
        let r = safe_resolve(dir.path(), "skills/sub/foo.md");
        assert!(r.is_ok());
        assert!(r.unwrap().strip_prefix(dir.path()).is_ok());
    }

    // --- end-to-end import ---

    fn write_skill_file(dir: &Path, rel: &str, body: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    #[test]
    fn import_creates_skills_with_namespaced_names_and_shipped_kind() {
        let store = fresh_store();
        let dir = tempdir().unwrap();
        write_skill_file(dir.path(), "skills/query-builder.md", "# How to query\n...");
        write_skill_file(dir.path(), "skills/migrate.md", "# How to migrate\n...");

        let decls = vec![
            SkillDecl {
                name: "Query Builder".into(),
                description: "Use to construct SQL queries against Postgres.".into(),
                entry: "skills/query-builder.md".into(),
                tags: vec!["db".into(), "sql".into()],
            },
            SkillDecl {
                name: "migrate".into(),
                description: "Use to plan and run database migrations.".into(),
                entry: "skills/migrate.md".into(),
                tags: vec!["db".into()],
            },
        ];
        let report = import_plugin_skills(&store, "postgres-toolkit", &decls, dir.path(), 1000);
        assert!(report.failed.is_empty(), "failures: {:?}", report.failed);
        assert_eq!(
            report.imported,
            vec!["postgres-toolkit/query-builder", "postgres-toolkit/migrate"]
        );

        let g = store
            .get("postgres-toolkit/query-builder")
            .unwrap()
            .unwrap();
        assert_eq!(g.registration_kind, RegistrationKind::Shipped);
        assert_eq!(g.owning_plugin_id.as_deref(), Some("postgres-toolkit"));
        assert_eq!(g.state, SkillState::Trial);
        assert!(g.current_version.body_md.contains("How to query"));
        // Frontmatter carries the namespaced name + tags.
        let fm: serde_json::Value =
            serde_json::from_str(&g.current_version.frontmatter_json).unwrap();
        assert_eq!(fm["name"], "postgres-toolkit/query-builder");
        assert_eq!(fm["tags"][0], "db");
    }

    #[test]
    fn import_then_re_import_appends_new_version() {
        let store = fresh_store();
        let dir = tempdir().unwrap();
        write_skill_file(dir.path(), "s.md", "v1 body");
        let decl = SkillDecl {
            name: "s".into(),
            description: "v1 desc".into(),
            entry: "s.md".into(),
            tags: vec![],
        };
        import_plugin_skills(&store, "p", &[decl.clone()], dir.path(), 1);
        // Bump the body and re-import (simulates plugin upgrade).
        write_skill_file(dir.path(), "s.md", "v2 body");
        let decl2 = SkillDecl {
            description: "v2 desc".into(),
            ..decl
        };
        let report = import_plugin_skills(&store, "p", &[decl2], dir.path(), 2);
        assert!(report.failed.is_empty());
        let g = store.get("p/s").unwrap().unwrap();
        assert_eq!(g.current_version.version, 2);
        assert_eq!(g.current_version.body_md, "v2 body");
        // Parent linkage preserved.
        assert!(g.current_version.parent_version_id.is_some());
    }

    #[test]
    fn import_conflict_with_admin_authored_skill_returns_already_exists() {
        let store = fresh_store();
        // Admin authored `dev/scaffold` directly.
        store
            .create(
                NewSkill {
                    name: "dev/scaffold".into(),
                    source: "admin:Controller".into(),
                    registration_kind: RegistrationKind::Authored,
                    owning_plugin_id: None,
                    initial_version: NewSkillVersion {
                        description: "admin's scaffold".into(),
                        body_md: "by admin".into(),
                        frontmatter_json: "{}".into(),
                        authored_by: "admin:Controller".into(),
                        promotion_notes: None,
                    },
                    resources: vec![],
                },
                crate::scanner::Strictness::Strict,
                100,
            )
            .unwrap();

        // Plugin `dev` ships a skill `scaffold` — namespaced name
        // collides with the admin-authored row.
        let dir = tempdir().unwrap();
        write_skill_file(dir.path(), "scaffold.md", "shipped body");
        let decls = vec![SkillDecl {
            name: "scaffold".into(),
            description: "shipped scaffold".into(),
            entry: "scaffold.md".into(),
            tags: vec![],
        }];
        let report = import_plugin_skills(&store, "dev", &decls, dir.path(), 200);
        assert_eq!(report.imported.len(), 0);
        assert_eq!(report.failed.len(), 1);
        assert!(matches!(
            report.failed[0].error,
            SkillError::AlreadyExists(_)
        ));
        // Admin's skill is unchanged.
        let g = store.get("dev/scaffold").unwrap().unwrap();
        assert_eq!(g.registration_kind, RegistrationKind::Authored);
        assert_eq!(g.current_version.body_md, "by admin");
    }

    #[test]
    fn import_reports_partial_success_when_one_skill_fails() {
        let store = fresh_store();
        let dir = tempdir().unwrap();
        write_skill_file(dir.path(), "good.md", "good body");
        // bad.md intentionally not created.
        let decls = vec![
            SkillDecl {
                name: "good".into(),
                description: "the good one".into(),
                entry: "good.md".into(),
                tags: vec![],
            },
            SkillDecl {
                name: "bad".into(),
                description: "missing entry file".into(),
                entry: "bad.md".into(),
                tags: vec![],
            },
        ];
        let report = import_plugin_skills(&store, "p", &decls, dir.path(), 1);
        assert_eq!(report.imported, vec!["p/good"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].plugin_local_name, "bad");
    }

    #[test]
    fn import_with_credential_in_body_is_rejected_by_scanner() {
        let store = fresh_store();
        let dir = tempdir().unwrap();
        write_skill_file(
            dir.path(),
            "leak.md",
            "use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to call",
        );
        let decls = vec![SkillDecl {
            name: "leak".into(),
            description: "tries to leak a key".into(),
            entry: "leak.md".into(),
            tags: vec![],
        }];
        let report = import_plugin_skills(&store, "p", &decls, dir.path(), 1);
        assert_eq!(report.imported.len(), 0);
        assert_eq!(report.failed.len(), 1);
        assert!(matches!(report.failed[0].error, SkillError::Blocked { .. }));
    }

    // --- archive cascade ---

    #[test]
    fn archive_for_plugin_archives_only_that_plugins_skills() {
        let store = fresh_store();
        let dir = tempdir().unwrap();
        write_skill_file(dir.path(), "a.md", "body a");
        write_skill_file(dir.path(), "b.md", "body b");
        import_plugin_skills(
            &store,
            "alpha",
            &[SkillDecl {
                name: "x".into(),
                description: "alpha skill".into(),
                entry: "a.md".into(),
                tags: vec![],
            }],
            dir.path(),
            1,
        );
        import_plugin_skills(
            &store,
            "beta",
            &[SkillDecl {
                name: "y".into(),
                description: "beta skill".into(),
                entry: "b.md".into(),
                tags: vec![],
            }],
            dir.path(),
            2,
        );

        let archived = store.archive_for_plugin("alpha", 100).unwrap();
        assert_eq!(archived, vec!["alpha/x"]);
        // alpha/x is now archived; beta/y survives.
        assert!(store.list_for_plugin("alpha").unwrap().is_empty());
        assert_eq!(store.list_for_plugin("beta").unwrap(), vec!["beta/y"]);
    }

    #[test]
    fn reinstall_after_uninstall_reactivates_archived_skills() {
        // Audit regression (2026-05-03): install → uninstall →
        // reinstall must end with the skill VISIBLE again. Earlier
        // version of import_shipped silently appended a new version
        // without flipping `state` from archived back to trial,
        // leaving the skill invisible to the agent surface.
        let store = fresh_store();
        let dir = tempdir().unwrap();
        write_skill_file(dir.path(), "s.md", "v1");
        let decl = SkillDecl {
            name: "s".into(),
            description: "describe".into(),
            entry: "s.md".into(),
            tags: vec![],
        };
        // First install.
        import_plugin_skills(&store, "p", &[decl.clone()], dir.path(), 1);
        assert_eq!(store.list_for_plugin("p").unwrap(), vec!["p/s"]);

        // Uninstall — archives the skill.
        store.archive_for_plugin("p", 2).unwrap();
        assert!(store.list_for_plugin("p").unwrap().is_empty());

        // Reinstall — must reactivate the row + add a new version.
        write_skill_file(dir.path(), "s.md", "v2");
        let report = import_plugin_skills(&store, "p", &[decl], dir.path(), 3);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.imported, vec!["p/s"]);

        // Active list contains the reactivated skill.
        assert_eq!(store.list_for_plugin("p").unwrap(), vec!["p/s"]);
        let g = store.get("p/s").unwrap().unwrap();
        assert_eq!(g.state, SkillState::Trial);
        assert!(g.archived_at.is_none());
        assert_eq!(g.current_version.body_md, "v2");
        // History preserved: this is the SECOND version of the row.
        assert_eq!(g.current_version.version, 2);
    }

    #[test]
    fn archive_for_plugin_is_idempotent_and_returns_empty_on_no_match() {
        let store = fresh_store();
        let archived = store.archive_for_plugin("ghost", 1).unwrap();
        assert!(archived.is_empty());
    }
}
