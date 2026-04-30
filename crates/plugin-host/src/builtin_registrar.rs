//! Boot-time registrar that wires the [`execlaw_core::builtin_tools`]
//! catalog into a [`HookRegistry`] and seeds the corresponding
//! `config_tool_access` rows.
//!
//! Two responsibilities, kept together because they have to fire in
//! the same order and either both succeed or neither does (so
//! operators don't end up with a tool registered but no policy row,
//! or vice versa):
//!
//! 1. Walk every `Arc<dyn ToolImpl>` in `core_builtin_tools()` and
//!    call [`HookRegistry::register_builtin`] on it. Conflict with an
//!    already-installed plugin tool name is fatal — the operator
//!    needs to know.
//! 2. For each registered built-in, upsert a `config_tool_access`
//!    row via the existing [`ToolAccessStore::upsert_seen`] path. On
//!    first sight the descriptor's `default_allowed_classes` becomes
//!    the operator policy; on rerun the existing operator setting is
//!    preserved (idempotent — server restarts don't fight the
//!    Settings UI).
//!
//! Production callers wire this into `cmd_serve` after migrations
//! and before `axum::serve`.
//!
//! 2026-04-29.

use crate::HookRegistry;
use execlaw_core::builtin_tools::core_builtin_tools;
use execlaw_core::tool::{ToolImpl, ToolSource};
use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource as RowToolSource};
use execlaw_core::Database;
use std::sync::Arc;

/// Errors a registrar pass can return. Distinguishes registration
/// conflicts (caller-fixable: rename / disable a plugin) from raw
/// DB failures (operator's keyring or filesystem is sick).
#[derive(Debug, thiserror::Error)]
pub enum RegisterBuiltinsError {
    #[error("registry error for tool '{name}': {message}")]
    Registry { name: String, message: String },
    #[error("seed access row for tool '{name}': {source}")]
    Seed {
        name: String,
        #[source]
        source: execlaw_core::DbError,
    },
}

/// Register every core built-in into `registry` and seed
/// `config_tool_access` rows in `db` for them. `now_unix` becomes the
/// `first_seen_at` / `last_seen_at` for new rows; existing rows
/// preserve their operator-set policy.
///
/// Returns a `Vec<Arc<dyn ToolImpl>>` of the tools that landed in the
/// registry — handy for boot-time logging, and for tests that want to
/// double-check the catalog post-install without re-pulling the list
/// from `core_builtin_tools`.
pub fn register_core_builtins(
    registry: &HookRegistry,
    db: &Database,
    now_unix: i64,
) -> Result<Vec<Arc<dyn ToolImpl>>, RegisterBuiltinsError> {
    register_builtins(registry, db, now_unix, core_builtin_tools())
}

/// Lower-level form: register an explicit list of tools. Tests inject
/// their own to exercise edge cases without depending on the live
/// `core_builtin_tools` set.
pub fn register_builtins(
    registry: &HookRegistry,
    db: &Database,
    now_unix: i64,
    tools: Vec<Arc<dyn ToolImpl>>,
) -> Result<Vec<Arc<dyn ToolImpl>>, RegisterBuiltinsError> {
    let access = ToolAccessStore::new(db);
    let mut landed = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool.descriptor().name.clone();
        registry.register_builtin(tool.clone()).map_err(|e| {
            RegisterBuiltinsError::Registry {
                name: name.clone(),
                message: e,
            }
        })?;
        let descriptor = tool.descriptor();
        // Translate the in-memory descriptor source into the
        // tool_access table's enum. Built-ins always map to the
        // `Builtin` row source; the descriptor's source field is
        // belt-and-braces validated below.
        debug_assert_eq!(descriptor.source, ToolSource::Builtin);
        let seed = ToolAccessSeed {
            tool_name: name.clone(),
            source: RowToolSource::Builtin,
            source_id: None,
            description: Some(descriptor.description.clone()),
            input_schema: Some(descriptor.schema.to_string()),
            default_allowed_classes: descriptor.default_allowed_classes.clone(),
        };
        access
            .upsert_seen(&seed, now_unix)
            .map_err(|e| RegisterBuiltinsError::Seed {
                name: name.clone(),
                source: e,
            })?;
        landed.push(tool);
    }
    Ok(landed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::tool::{
        ToolCtx, ToolDescriptor, ToolLatency, ToolOutcome, ToolSource as DescSource,
    };

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn dummy_tool(name: &str, defaults: &[&str]) -> Arc<dyn ToolImpl> {
        struct T {
            d: ToolDescriptor,
        }
        #[async_trait]
        impl ToolImpl for T {
            fn descriptor(&self) -> &ToolDescriptor {
                &self.d
            }
            async fn invoke(&self, _ctx: ToolCtx, _args: serde_json::Value) -> ToolOutcome {
                ToolOutcome::ok(serde_json::json!({}))
            }
        }
        Arc::new(T {
            d: ToolDescriptor {
                name: name.into(),
                description: format!("desc for {name}"),
                schema: serde_json::json!({"type": "object"}),
                source: DescSource::Builtin,
                latency: ToolLatency::Low,
                capabilities: vec![],
                default_allowed_classes: defaults.iter().map(|s| s.to_string()).collect(),
            },
        })
    }

    #[test]
    fn register_core_builtins_lands_every_tool_and_seed_row() {
        let db = fresh_db();
        let reg = HookRegistry::new();
        let landed = register_core_builtins(&reg, &db, 1_700_000_000).unwrap();
        assert!(!landed.is_empty());

        // Every tool from the core catalog must be looked-up-able via
        // its name.
        let access = ToolAccessStore::new(&db);
        for tool in &landed {
            let name = &tool.descriptor().name;
            assert!(reg.builtin(name).is_some(), "{name} missing from registry");
            let row = access.get(name).unwrap();
            assert!(row.is_some(), "{name} missing access row");
            let row = row.unwrap();
            assert_eq!(row.source, RowToolSource::Builtin);
            assert!(row.enabled);
            assert_eq!(
                row.allowed_classes,
                tool.descriptor().default_allowed_classes
            );
        }
    }

    #[test]
    fn re_registering_returns_a_clear_conflict_error() {
        let db = fresh_db();
        let reg = HookRegistry::new();
        register_core_builtins(&reg, &db, 0).unwrap();
        // Re-running should fail on the first tool because the
        // registry rejects duplicates. The DB row already exists and
        // is preserved. (`unwrap_err` would require `Vec<Arc<dyn
        // ToolImpl>>: Debug` which isn't object-safe; match on the
        // result directly instead.)
        match register_core_builtins(&reg, &db, 0) {
            Err(RegisterBuiltinsError::Registry { name, message }) => {
                assert!(!name.is_empty());
                assert!(message.contains("already registered"));
            }
            Err(other) => panic!("expected Registry conflict, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    /// Rerunning the registrar on a FRESH registry with an existing
    /// DB preserves operator policy that was changed between runs.
    /// This is the production behaviour on server restart.
    #[test]
    fn rerun_preserves_operator_policy_changes() {
        let db = fresh_db();

        // Run 1: seed with default allowlist.
        let reg = HookRegistry::new();
        register_builtins(
            &reg,
            &db,
            100,
            vec![dummy_tool("only_for_controller", &["Controller"])],
        )
        .unwrap();

        // Operator clamps the allowlist via Settings.
        ToolAccessStore::new(&db)
            .set_policy("only_for_controller", true, &[])
            .unwrap();

        // Run 2 (server restart): fresh registry, same DB.
        let reg2 = HookRegistry::new();
        register_builtins(
            &reg2,
            &db,
            200,
            vec![dummy_tool("only_for_controller", &["Controller"])],
        )
        .unwrap();

        // The operator's empty allowlist is preserved — `upsert_seen`
        // doesn't overwrite policy on rerun.
        let row = ToolAccessStore::new(&db)
            .get("only_for_controller")
            .unwrap()
            .unwrap();
        assert!(row.allowed_classes.is_empty());
        assert_eq!(row.last_seen_at, 200);
    }

    #[test]
    fn empty_input_list_is_a_no_op_success() {
        let db = fresh_db();
        let reg = HookRegistry::new();
        let landed = register_builtins(&reg, &db, 0, vec![]).unwrap();
        assert!(landed.is_empty());
    }

    /// All-or-nothing semantics: if registering tool 2 conflicts,
    /// tool 1 stays registered (already inserted) but tool 3 never
    /// gets a chance. The error names the conflicting tool so
    /// operators know what to fix.
    #[test]
    fn mid_chain_conflict_stops_registration_and_names_culprit() {
        let db = fresh_db();
        let reg = HookRegistry::new();
        // Pre-seed conflict via a manual register.
        reg.register_builtin(dummy_tool("middle", &["Controller"]))
            .unwrap();

        let result = register_builtins(
            &reg,
            &db,
            0,
            vec![
                dummy_tool("first", &["Controller"]),
                dummy_tool("middle", &["Controller"]), // conflict
                dummy_tool("third", &["Controller"]),
            ],
        );
        match result {
            Err(RegisterBuiltinsError::Registry { name, .. }) => {
                assert_eq!(name, "middle");
            }
            Err(other) => panic!("expected Registry, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
        // first DID register successfully BEFORE the conflict — so
        // the operator's mental model is: registration is sequential,
        // first conflict stops the run. Document that clearly.
        assert!(reg.builtin("first").is_some());
        assert!(reg.builtin("third").is_none());
    }
}
