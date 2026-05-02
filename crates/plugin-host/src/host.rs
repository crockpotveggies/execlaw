//! `PluginHost` — the lifecycle orchestrator.
//!
//! The host ties four primitives together:
//!
//! 1. [`execlaw_plugin_sdk::zip_stage`] — ZIP upload staging.
//! 2. [`execlaw_plugin_sdk::PluginManifest`] — `plugin.toml` parsing.
//! 3. [`crate::hook_registry::HookRegistry`] — per-hook lookup maps
//!    that runtime consumers (`TurnExecutor`, chat UI, etc.) query.
//! 4. [`crate::subprocess::SubprocessPlugin`] — the isolation tier
//!    Phase 2 actually ships.
//!
//! On top of those, the host:
//!
//! - persists install state in the `state_plugins` SQLite table so
//!   installs survive restart (§4, Phase 2);
//! - runs the hook-registration + subprocess-spawn flow atomically
//!   (all-or-nothing — a failed spawn un-registers the hooks);
//! - routes `ToolDispatch::call` from the runner into the
//!   subprocess's JSON-RPC channel when the tool belongs to a plugin;
//! - enforces the capability contract at dispatch time: a caller
//!   whose capability set doesn't cover the tool's
//!   `required_capabilities` gets rejected before the child sees the
//!   args (§7.2 + §7.3).

use crate::hook_registry::HookRegistry;
use crate::subprocess::{SubprocessPlugin, SubprocessSpec};
use async_trait::async_trait;
use execlaw_core::db::{Database, DbError};
use execlaw_plugin_sdk::PluginManifest;
use rusqlite::params;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum PluginHostError {
    #[error("plugin '{0}' not installed")]
    NotInstalled(String),
    #[error("plugin '{0}' already installed — uninstall first to upgrade")]
    AlreadyInstalled(String),
    #[error("hook-registry conflict: {0}")]
    HookConflict(String),
    #[error("manifest parse failed: {0}")]
    Manifest(String),
    #[error("subprocess spawn failed: {0}")]
    Spawn(String),
    #[error("unsupported runtime tier '{0}' — Phase 2 supports 'subprocess' only")]
    UnsupportedTier(String),
    #[error("plugin declares tools/transport but has no [runtime] table")]
    MissingRuntime,
    #[error("db: {0}")]
    Db(#[from] DbError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Persisted row shape for `state_plugins`.
#[derive(Debug, Clone)]
pub struct PluginRow {
    pub plugin_id: String,
    pub version: String,
    pub manifest_toml: String,
    pub stage_path: String,
    pub enabled: bool,
    pub installed_at: i64,
    pub updated_at: i64,
}

/// Orchestrator. Cheap to clone; shares one `HookRegistry` and one
/// subprocess map across route handlers.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<PluginHostInner>,
}

struct PluginHostInner {
    db: Database,
    registry: HookRegistry,
    subprocesses: RwLock<BTreeMap<String, Arc<SubprocessPlugin>>>,
    /// Per-plugin Rhai scripts. Empty for subprocess-tier plugins;
    /// populated at enable / hydrate time for `tier = "script"`.
    script_plugins: RwLock<BTreeMap<String, execlaw_script::ScriptPlugin>>,
    /// Shared engine factory — cheap to clone, builds a fresh
    /// per-plugin Rhai engine on demand.
    script_engine: execlaw_script::ScriptEngine,
    /// Root directory for staged plugin ZIPs. Per-install directories
    /// land under `<root>/<plugin_id>-<version>/`.
    stage_root: PathBuf,
}

impl PluginHost {
    pub fn new(db: Database, registry: HookRegistry, stage_root: PathBuf) -> Self {
        Self {
            inner: Arc::new(PluginHostInner {
                db,
                registry,
                subprocesses: RwLock::new(BTreeMap::new()),
                script_plugins: RwLock::new(BTreeMap::new()),
                script_engine: execlaw_script::ScriptEngine::new(),
                stage_root,
            }),
        }
    }

    pub fn registry(&self) -> &HookRegistry {
        &self.inner.registry
    }

    pub fn stage_root(&self) -> &Path {
        &self.inner.stage_root
    }

    /// Database handle the host was constructed with. Used by Phase-8a
    /// callers (the `ChainedToolDispatch` access gate) that need to
    /// read `config_tool_access` rows alongside dispatching tools.
    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    /// Install a plugin from an already-staged directory.
    ///
    /// The caller is expected to have staged the ZIP via
    /// `execlaw_plugin_sdk::zip_stage::stage_zip` into a directory
    /// under [`stage_root`](Self::stage_root).
    ///
    /// This function:
    ///
    /// 1. Parses `plugin.toml` from `stage_path`.
    /// 2. Rejects if a plugin with this id already exists.
    /// 3. Registers every declared hook (all-or-nothing per
    ///    [`HookRegistry::enable`]).
    /// 4. Spawns the subprocess when the manifest declares a tool or
    ///    a transport; if spawn fails, un-registers the hooks.
    /// 5. Persists the install row in `state_plugins`.
    pub async fn install(&self, stage_path: &Path) -> Result<PluginRow, PluginHostError> {
        let manifest_path = stage_path.join("plugin.toml");
        let manifest_toml = std::fs::read_to_string(&manifest_path)
            .map_err(|e| PluginHostError::Manifest(format!("read plugin.toml: {e}")))?;
        let manifest = PluginManifest::parse(&manifest_toml)
            .map_err(|e| PluginHostError::Manifest(e.to_string()))?;
        let plugin_id = manifest.plugin.id.clone();
        let version = manifest.plugin.version.clone();

        // Already installed?
        if self.get_row(&plugin_id)?.is_some() {
            return Err(PluginHostError::AlreadyInstalled(plugin_id));
        }

        // Step 1 — register hooks.
        self.inner
            .registry
            .enable(&manifest)
            .map_err(PluginHostError::HookConflict)?;

        // Step 2 — spin up the per-tier runtime (subprocess child or
        // script engine) when the manifest declares a hook that
        // needs one. If launch fails we un-register the hooks so
        // the registry doesn't leak a plugin that can't serve.
        let needs_runtime = !manifest.tools.is_empty()
            || manifest.transport.is_some()
            || manifest.identity_provider.is_some();
        if needs_runtime {
            let runtime = manifest
                .runtime
                .as_ref()
                .ok_or(PluginHostError::MissingRuntime)?;
            let tier = runtime.parsed_tier().ok_or_else(|| {
                self.inner.registry.disable(&plugin_id);
                PluginHostError::UnsupportedTier(runtime.tier.clone())
            })?;
            match tier {
                execlaw_plugin_sdk::manifest::RuntimeTier::Subprocess => {
                    let spec = SubprocessSpec {
                        plugin_id: plugin_id.clone(),
                        executable: resolve_executable(
                            stage_path,
                            runtime_executable_or_err(runtime)?,
                        ),
                        args: runtime.args.clone(),
                        cwd: Some(stage_path.to_path_buf()),
                    };
                    let plugin = match SubprocessPlugin::spawn(spec).await {
                        Ok(p) => p,
                        Err(e) => {
                            self.inner.registry.disable(&plugin_id);
                            return Err(PluginHostError::Spawn(e));
                        }
                    };
                    self.inner
                        .subprocesses
                        .write()
                        .await
                        .insert(plugin_id.clone(), Arc::new(plugin));
                }
                execlaw_plugin_sdk::manifest::RuntimeTier::Script => {
                    let source_rel = runtime_source_or_err(runtime).map_err(|e| {
                        self.inner.registry.disable(&plugin_id);
                        e
                    })?;
                    let source_path = stage_path.join(source_rel);
                    let script = match execlaw_script::ScriptPlugin::from_file(
                        &plugin_id,
                        &source_path,
                        &self.inner.script_engine,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            self.inner.registry.disable(&plugin_id);
                            return Err(PluginHostError::Spawn(format!(
                                "script load: {e}"
                            )));
                        }
                    };
                    self.inner
                        .script_plugins
                        .write()
                        .await
                        .insert(plugin_id.clone(), script);
                }
            }
        }

        // Step 3 — persist install row.
        let now = chrono::Utc::now().timestamp();
        let row = PluginRow {
            plugin_id: plugin_id.clone(),
            version: version.clone(),
            manifest_toml: manifest_toml.clone(),
            stage_path: stage_path.to_string_lossy().into_owned(),
            enabled: true,
            installed_at: now,
            updated_at: now,
        };
        self.insert_row(&row)?;
        info!(plugin_id, version, "plugin installed");
        Ok(row)
    }

    /// Uninstall: disable hooks, kill subprocess, remove DB row +
    /// staged directory. Idempotent — missing plugin returns
    /// `NotInstalled`.
    pub async fn uninstall(&self, plugin_id: &str) -> Result<(), PluginHostError> {
        let row = self
            .get_row(plugin_id)?
            .ok_or_else(|| PluginHostError::NotInstalled(plugin_id.to_owned()))?;

        self.inner.registry.disable(plugin_id);

        if let Some(plugin) = self
            .inner
            .subprocesses
            .write()
            .await
            .remove(plugin_id)
        {
            plugin.shutdown().await;
        }
        let _ = self
            .inner
            .script_plugins
            .write()
            .await
            .remove(plugin_id);

        self.delete_row(plugin_id)?;

        // Best-effort remove the staged directory.
        if let Err(e) = std::fs::remove_dir_all(&row.stage_path) {
            warn!(plugin_id, path = %row.stage_path, error = %e, "failed to remove staged dir");
        }
        info!(plugin_id, "plugin uninstalled");
        Ok(())
    }

    /// Disable without uninstalling. Hooks come off the registry and
    /// the subprocess is killed, but the DB row + stage dir stay so
    /// re-enable is cheap.
    pub async fn disable(&self, plugin_id: &str) -> Result<(), PluginHostError> {
        let Some(mut row) = self.get_row(plugin_id)? else {
            return Err(PluginHostError::NotInstalled(plugin_id.to_owned()));
        };
        if !row.enabled {
            return Ok(()); // idempotent
        }
        self.inner.registry.disable(plugin_id);
        if let Some(plugin) = self
            .inner
            .subprocesses
            .write()
            .await
            .remove(plugin_id)
        {
            plugin.shutdown().await;
        }
        // Drop the script plugin (if any). No subprocess to kill —
        // the rhai::Engine is just memory.
        let _ = self
            .inner
            .script_plugins
            .write()
            .await
            .remove(plugin_id);
        row.enabled = false;
        row.updated_at = chrono::Utc::now().timestamp();
        self.update_row(&row)?;
        info!(plugin_id, "plugin disabled");
        Ok(())
    }

    /// Re-enable a previously disabled plugin: parse manifest from
    /// the persisted `manifest_toml`, re-register hooks, re-spawn the
    /// subprocess if needed.
    pub async fn enable(&self, plugin_id: &str) -> Result<(), PluginHostError> {
        let Some(mut row) = self.get_row(plugin_id)? else {
            return Err(PluginHostError::NotInstalled(plugin_id.to_owned()));
        };
        if row.enabled {
            return Ok(()); // idempotent
        }
        let manifest = PluginManifest::parse(&row.manifest_toml)
            .map_err(|e| PluginHostError::Manifest(e.to_string()))?;
        self.inner
            .registry
            .enable(&manifest)
            .map_err(PluginHostError::HookConflict)?;

        let needs_runtime = !manifest.tools.is_empty()
            || manifest.transport.is_some()
            || manifest.identity_provider.is_some();
        if needs_runtime {
            let runtime = manifest
                .runtime
                .as_ref()
                .ok_or(PluginHostError::MissingRuntime)?;
            let tier = runtime
                .parsed_tier()
                .ok_or_else(|| PluginHostError::UnsupportedTier(runtime.tier.clone()))?;
            let stage = PathBuf::from(&row.stage_path);
            match tier {
                execlaw_plugin_sdk::manifest::RuntimeTier::Subprocess => {
                    let spec = SubprocessSpec {
                        plugin_id: plugin_id.to_owned(),
                        executable: resolve_executable(
                            &stage,
                            runtime_executable_or_err(runtime)?,
                        ),
                        args: runtime.args.clone(),
                        cwd: Some(stage),
                    };
                    let plugin = SubprocessPlugin::spawn(spec)
                        .await
                        .map_err(PluginHostError::Spawn)?;
                    self.inner
                        .subprocesses
                        .write()
                        .await
                        .insert(plugin_id.to_owned(), Arc::new(plugin));
                }
                execlaw_plugin_sdk::manifest::RuntimeTier::Script => {
                    let source_path = stage.join(runtime_source_or_err(runtime)?);
                    let script = execlaw_script::ScriptPlugin::from_file(
                        plugin_id,
                        &source_path,
                        &self.inner.script_engine,
                    )
                    .map_err(|e| PluginHostError::Spawn(format!("script load: {e}")))?;
                    self.inner
                        .script_plugins
                        .write()
                        .await
                        .insert(plugin_id.to_owned(), script);
                }
            }
        }

        row.enabled = true;
        row.updated_at = chrono::Utc::now().timestamp();
        self.update_row(&row)?;
        info!(plugin_id, "plugin enabled");
        Ok(())
    }

    /// On server boot — re-hydrate every `enabled = 1` plugin from
    /// the DB by re-registering hooks and (for subprocess-tier)
    /// respawning the child.
    pub async fn hydrate(&self) -> Result<(), PluginHostError> {
        let rows = self.list_rows()?;
        for row in rows.into_iter().filter(|r| r.enabled) {
            let manifest = match PluginManifest::parse(&row.manifest_toml) {
                Ok(m) => m,
                Err(e) => {
                    warn!(plugin_id = %row.plugin_id, error = %e, "skipping plugin with unparseable manifest");
                    continue;
                }
            };
            if let Err(e) = self.inner.registry.enable(&manifest) {
                warn!(plugin_id = %row.plugin_id, error = %e, "skipping plugin with hook conflict on hydrate");
                continue;
            }
            let needs_runtime = !manifest.tools.is_empty()
            || manifest.transport.is_some()
            || manifest.identity_provider.is_some();
            if needs_runtime {
                if let Some(runtime) = &manifest.runtime {
                    let stage = PathBuf::from(&row.stage_path);
                    match runtime.parsed_tier() {
                        Some(execlaw_plugin_sdk::manifest::RuntimeTier::Subprocess) => {
                            let exe = match runtime_executable_or_err(runtime) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };
                            let spec = SubprocessSpec {
                                plugin_id: row.plugin_id.clone(),
                                executable: resolve_executable(&stage, exe),
                                args: runtime.args.clone(),
                                cwd: Some(stage),
                            };
                            match SubprocessPlugin::spawn(spec).await {
                                Ok(p) => {
                                    self.inner
                                        .subprocesses
                                        .write()
                                        .await
                                        .insert(row.plugin_id.clone(), Arc::new(p));
                                    debug!(plugin_id = %row.plugin_id, "hydrated subprocess plugin");
                                }
                                Err(e) => {
                                    warn!(plugin_id = %row.plugin_id, error = %e, "failed to respawn subprocess on hydrate");
                                    self.inner.registry.disable(&row.plugin_id);
                                }
                            }
                        }
                        Some(execlaw_plugin_sdk::manifest::RuntimeTier::Script) => {
                            let src = match runtime_source_or_err(runtime) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };
                            let path = stage.join(src);
                            match execlaw_script::ScriptPlugin::from_file(
                                &row.plugin_id,
                                &path,
                                &self.inner.script_engine,
                            ) {
                                Ok(s) => {
                                    self.inner
                                        .script_plugins
                                        .write()
                                        .await
                                        .insert(row.plugin_id.clone(), s);
                                    debug!(plugin_id = %row.plugin_id, "hydrated script plugin");
                                }
                                Err(e) => {
                                    warn!(plugin_id = %row.plugin_id, error = %e, "failed to load script on hydrate");
                                    self.inner.registry.disable(&row.plugin_id);
                                }
                            }
                        }
                        None => {
                            warn!(plugin_id = %row.plugin_id, tier = %runtime.tier, "unknown tier; skipping hydrate");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Query every registered identity-provider plugin to resolve a
    /// transport identifier (`{transport, handle}`) into a set of
    /// potential matches (§2.14).
    ///
    /// Each provider responds to the JSON-RPC `identity.resolve`
    /// method with either `{match: {...IdentityMatch fields}}` or
    /// `{match: null}` for no match. Providers that fail (timeout,
    /// parse error, subprocess crashed) are skipped with a warning;
    /// a single bad provider must not block identity resolution for
    /// the rest.
    ///
    /// Returns every match; the caller decides how to rank (typically
    /// highest-confidence wins, with tie-break on registration order).
    pub async fn resolve_identity(
        &self,
        transport: &str,
        handle: &str,
    ) -> Vec<serde_json::Value> {
        let providers = self.inner.registry.identity_providers();
        if providers.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        let base_params = serde_json::json!({
            "transport": transport,
            "handle": handle,
        });
        // Take both maps once at the top — drop the locks before
        // we await on any plugin call so the dispatch path stays
        // re-entrant.
        let subs = {
            let g = self.inner.subprocesses.read().await;
            g.clone()
        };
        let scripts = {
            let g = self.inner.script_plugins.read().await;
            g.clone()
        };
        for provider in &providers {
            let mut params = base_params.clone();
            self.inject_oauth_tokens(&provider.plugin_id, &mut params);
            let result = if let Some(plugin) = subs.get(&provider.plugin_id) {
                plugin.call("identity.resolve", params).await
            } else if let Some(script) = scripts.get(&provider.plugin_id) {
                let oauth = oauth_map_from_params(&params);
                script
                    .identity_resolve(transport, handle, oauth)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                // Provider registered but no live runtime —
                // declares-no-runtime plugin or hydration race.
                continue;
            };
            match result {
                Ok(value) => {
                    if let Some(m) = value.get("match") {
                        if !m.is_null() {
                            matches.push(m.clone());
                        }
                    }
                }
                Err(e) => warn!(
                    plugin_id = %provider.plugin_id,
                    error = %e,
                    "identity.resolve failed; skipping provider"
                ),
            }
        }
        matches
    }

    /// Call a tool exposed by an installed plugin. Returns the JSON
    /// result on success or a structured error string the caller can
    /// encode into a `ToolResultPayload::outcome = Err(_)`.
    ///
    /// **Capability enforcement**: `caller_caps` must be a superset
    /// of the tool's `required_capabilities`, or the call is rejected
    /// before the child sees any args. The wildcard `"*"` satisfies
    /// any requirement (used by Controller turns).
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        caller_caps: &[&str],
    ) -> Result<serde_json::Value, String> {
        let Some(registered) = self.inner.registry.tool(tool_name) else {
            return Err(format!("tool '{tool_name}' not registered"));
        };

        // Capability check — "*" is a wildcard (Controller grant).
        let has_wildcard = caller_caps.contains(&"*");
        if !has_wildcard {
            for required in &registered.required_capabilities {
                if !caller_caps.iter().any(|c| c == required) {
                    return Err(format!(
                        "tool '{tool_name}' requires capability '{required}' not in caller's set"
                    ));
                }
            }
        }

        // Dispatch by tier: try subprocess first, then script.
        let mut rpc_params = serde_json::json!({
            "tool": tool_name,
            "args": args.clone(),
        });
        self.inject_oauth_tokens(&registered.plugin_id, &mut rpc_params);
        let plugin = {
            let subs = self.inner.subprocesses.read().await;
            subs.get(&registered.plugin_id).cloned()
        };
        if let Some(plugin) = plugin {
            return plugin.call("tool.call", rpc_params).await;
        }
        let script = {
            let scripts = self.inner.script_plugins.read().await;
            scripts.get(&registered.plugin_id).cloned()
        };
        if let Some(script) = script {
            let oauth = oauth_map_from_params(&rpc_params);
            return script
                .tool_call(tool_name, args, oauth)
                .await
                .map_err(|e| e.to_string());
        }
        Err(format!(
            "plugin '{}' is registered but no runtime is loaded",
            registered.plugin_id
        ))
    }

    /// Look up every `[[oauth_accounts]]` declared by `plugin_id`'s
    /// manifest, fetch the current access_token from
    /// `state_oauth_tokens`. Returns a map of account_name →
    /// access_token. Empty when the plugin declares no accounts,
    /// is uninstalled / disabled, or none of its accounts have
    /// tokens persisted.
    ///
    /// Hot path on every tool.call / identity.resolve. The
    /// HookRegistry caches the manifest's [[oauth_accounts]] at
    /// enable-time so this method only does a sub-µs registry read
    /// + one indexed SQL SELECT per account.
    pub fn oauth_tokens_for(&self, plugin_id: &str) -> serde_json::Map<String, serde_json::Value> {
        let accounts = self.inner.registry.oauth_accounts_for(plugin_id);
        if accounts.is_empty() {
            return serde_json::Map::new();
        }
        let store = execlaw_core::oauth::OauthTokenStore::new(&self.inner.db);
        let mut tokens_map = serde_json::Map::new();
        for acc in &accounts {
            if let Ok(Some(t)) = store.get(plugin_id, &acc.account_name) {
                tokens_map.insert(
                    acc.account_name.clone(),
                    serde_json::Value::String(t.access_token),
                );
            }
        }
        tokens_map
    }

    /// Stitch `oauth_tokens_for(plugin_id)` into `params` under the
    /// reserved `_oauth` key. Plugins read their token from there
    /// without ever seeing the refresh_token or client_secret.
    fn inject_oauth_tokens(&self, plugin_id: &str, params: &mut serde_json::Value) {
        let tokens_map = self.oauth_tokens_for(plugin_id);
        if tokens_map.is_empty() {
            return;
        }
        if let Some(obj) = params.as_object_mut() {
            obj.insert("_oauth".into(), serde_json::Value::Object(tokens_map));
        }
    }

    // ---- DB row helpers (pure SQLite plumbing) --------------------

    pub fn list_rows(&self) -> Result<Vec<PluginRow>, PluginHostError> {
        self.inner.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT plugin_id, version, manifest_toml, stage_path, enabled, \
                        installed_at, updated_at \
                 FROM state_plugins ORDER BY plugin_id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(PluginRow {
                        plugin_id: r.get(0)?,
                        version: r.get(1)?,
                        manifest_toml: r.get(2)?,
                        stage_path: r.get(3)?,
                        enabled: r.get::<_, i64>(4)? != 0,
                        installed_at: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, DbError>(rows)
        }).map_err(PluginHostError::Db)
    }

    pub fn get_row(&self, plugin_id: &str) -> Result<Option<PluginRow>, PluginHostError> {
        self.inner.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT plugin_id, version, manifest_toml, stage_path, enabled, \
                            installed_at, updated_at \
                     FROM state_plugins WHERE plugin_id = ?1",
                    params![plugin_id],
                    |r| {
                        Ok(PluginRow {
                            plugin_id: r.get(0)?,
                            version: r.get(1)?,
                            manifest_toml: r.get(2)?,
                            stage_path: r.get(3)?,
                            enabled: r.get::<_, i64>(4)? != 0,
                            installed_at: r.get(5)?,
                            updated_at: r.get(6)?,
                        })
                    },
                )
                .ok();
            Ok(got)
        }).map_err(PluginHostError::Db)
    }

    fn insert_row(&self, row: &PluginRow) -> Result<(), PluginHostError> {
        self.inner.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_plugins \
                 (plugin_id, version, manifest_toml, stage_path, enabled, installed_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.plugin_id,
                    row.version,
                    row.manifest_toml,
                    row.stage_path,
                    row.enabled as i64,
                    row.installed_at,
                    row.updated_at,
                ],
            )?;
            Ok(())
        }).map_err(PluginHostError::Db)
    }

    fn update_row(&self, row: &PluginRow) -> Result<(), PluginHostError> {
        self.inner.db.with_conn(|c| {
            c.execute(
                "UPDATE state_plugins SET enabled = ?1, updated_at = ?2 WHERE plugin_id = ?3",
                params![row.enabled as i64, row.updated_at, row.plugin_id],
            )?;
            Ok(())
        }).map_err(PluginHostError::Db)
    }

    fn delete_row(&self, plugin_id: &str) -> Result<(), PluginHostError> {
        self.inner.db.with_conn(|c| {
            c.execute(
                "DELETE FROM state_plugins WHERE plugin_id = ?1",
                params![plugin_id],
            )?;
            Ok(())
        }).map_err(PluginHostError::Db)
    }
}

// ---------------------------------------------------------------------------
// Built-in-tools escape hatch (for the memory tool + any other
// runner-native tools). The server crate wires a `PluginHost` + a
// `BuiltinTools` impl into an `Arc<dyn ToolDispatch>` — keeping the
// `runner_local` trait out of this crate avoids a dep cycle.
// ---------------------------------------------------------------------------

/// Resolve a manifest-declared executable against the staged plugin
/// directory.
///
/// - `./foo` / `./foo.exe` → absolute path under the stage dir.
/// - A bare name like `node` or `sh` → left as-is so the OS resolves
///   it against `PATH` (the common case for Node-based plugins).
/// - An already-absolute path → left as-is.
///
/// Windows's `Command::new` does not look in CWD for relative paths;
/// this function normalizes that so plugin authors don't have to
/// worry about platform differences.
/// Pull the executable string out of a RuntimeDecl when the caller
/// expects a subprocess plugin. Manifest validation already
/// guarantees `executable` is present for `tier = "subprocess"`,
/// so this returns `MissingRuntime` only on a corrupt persisted
/// row that bypassed validation.
fn runtime_executable_or_err(
    runtime: &execlaw_plugin_sdk::manifest::RuntimeDecl,
) -> Result<&str, PluginHostError> {
    runtime
        .executable
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or(PluginHostError::MissingRuntime)
}

/// Mirror of `runtime_executable_or_err` for the script tier's
/// `source` field. Validation enforces presence at parse time;
/// this exists so `enable` / `hydrate` give a clean error if a
/// row sneaks through with NULL.
fn runtime_source_or_err(
    runtime: &execlaw_plugin_sdk::manifest::RuntimeDecl,
) -> Result<&str, PluginHostError> {
    runtime
        .source
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or(PluginHostError::MissingRuntime)
}

/// Pull the `_oauth` map out of the JSON params the host injected
/// for subprocess plugins. Script plugins receive it as a typed
/// `serde_json::Map`. Empty map when no OAuth accounts apply.
fn oauth_map_from_params(
    params: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    params
        .get("_oauth")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

fn resolve_executable(stage: &Path, declared: &str) -> String {
    let p = Path::new(declared);
    // Absolute path — use as-is.
    if p.is_absolute() {
        return declared.to_owned();
    }
    // Explicitly plugin-relative (starts with `./` or `.\`).
    let stripped = declared
        .strip_prefix("./")
        .or_else(|| declared.strip_prefix(".\\"));
    if let Some(rel) = stripped {
        // On Windows, prepend `.exe` if the target doesn't exist but a
        // `.exe` variant does — lets manifests stay cross-platform.
        let mut abs = stage.join(rel);
        if cfg!(windows) && !abs.exists() && abs.extension().is_none() {
            abs.set_extension("exe");
        }
        return abs.to_string_lossy().into_owned();
    }
    // Bare name — rely on PATH resolution.
    declared.to_owned()
}

/// Trait the server implements for built-in tools (e.g. the memory
/// shim) that don't live in a plugin. The adapter in the server
/// crate chains: try built-in → try plugin → fail.
#[async_trait]
pub trait BuiltinTools: Send + Sync {
    /// Return `Some(result)` if the tool is handled here; `None` if
    /// the caller should fall through to the plugin registry.
    async fn call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::oauth::{OauthClient, OauthClientStore, OauthTokenStore, OauthTokens};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    /// Insert an enabled plugin row + register hooks with the
    /// given registry. Skips the ZIP/spawn dance so we can test
    /// the read path directly.
    fn install_manifest(
        db: &Database,
        registry: &HookRegistry,
        plugin_id: &str,
        manifest_toml: &str,
    ) {
        let now = chrono::Utc::now().timestamp();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_plugins(plugin_id, version, manifest_toml, stage_path, enabled, installed_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                params![plugin_id, "0.1.0", manifest_toml, "/nonexistent", now],
            )?;
            Ok(())
        })
        .unwrap();
        let manifest = PluginManifest::parse(manifest_toml).unwrap();
        registry.enable(&manifest).unwrap();
    }

    #[test]
    fn oauth_tokens_for_returns_empty_when_plugin_has_no_oauth_accounts() {
        let db = fresh_db();
        let registry = HookRegistry::new();
        install_manifest(
            &db,
            &registry,
            "no-oauth",
            "[plugin]\nid=\"no-oauth\"\nname=\"No OAuth\"\nversion=\"0.1.0\"\ndescription=\"x\"\nauthor=\"a\"\nlicense=\"x\"\n\n[runtime]\ntier=\"subprocess\"\nexecutable=\"./bin\"\n",
        );
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));
        let map = host.oauth_tokens_for("no-oauth");
        assert!(map.is_empty());
    }

    #[test]
    fn oauth_tokens_for_returns_empty_when_no_tokens_persisted() {
        // Manifest declares accounts but the operator hasn't
        // connected any of them yet.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let manifest = r#"
[plugin]
id = "p-google"
name = "Google"
version = "0.1.0"
description = "x"
author = "a"
license = "x"

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["https://www.googleapis.com/auth/contacts.readonly"]

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        install_manifest(&db, &registry, "p-google", manifest);
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));
        assert!(host.oauth_tokens_for("p-google").is_empty());
    }

    #[test]
    fn oauth_tokens_for_returns_access_tokens_keyed_by_account_name() {
        let db = fresh_db();
        let manifest = r#"
[plugin]
id = "p-google"
name = "Google"
version = "0.1.0"
description = "x"
author = "a"
license = "x"

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["https://www.googleapis.com/auth/contacts.readonly"]

[[oauth_accounts]]
name = "team"
provider = "google"
scopes = ["https://www.googleapis.com/auth/contacts.readonly"]

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        let registry = HookRegistry::new();
        install_manifest(&db, &registry, "p-google", manifest);
        let now = chrono::Utc::now().timestamp();
        // Seed client + tokens for both accounts.
        for acc in &["controller", "team"] {
            OauthClientStore::new(&db)
                .upsert(&OauthClient {
                    plugin_id: "p-google".into(),
                    account_name: (*acc).into(),
                    provider: "google".into(),
                    client_id: "cid".into(),
                    client_secret: "secret".into(),
                    redirect_uri: "http://x".into(),
                    scopes_json: "[]".into(),
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            OauthTokenStore::new(&db)
                .upsert(&OauthTokens {
                    plugin_id: "p-google".into(),
                    account_name: (*acc).into(),
                    access_token: format!("ya29.{acc}"),
                    refresh_token: Some("rt".into()),
                    token_expires_at: now + 3600,
                    scopes_granted: "[]".into(),
                    account_email: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
        }
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));
        let map = host.oauth_tokens_for("p-google");
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("controller").and_then(|v| v.as_str()),
            Some("ya29.controller"),
        );
        assert_eq!(
            map.get("team").and_then(|v| v.as_str()),
            Some("ya29.team"),
        );
    }

    #[test]
    fn oauth_tokens_for_drops_after_registry_disable() {
        // Disabling a plugin removes its [[oauth_accounts]] from the
        // registry cache; oauth_tokens_for should immediately return
        // empty even if the underlying tokens are still in the
        // database.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let manifest = r#"
[plugin]
id = "p-google"
name = "Google"
version = "0.1.0"
description = "x"
author = "a"
license = "x"

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["https://www.googleapis.com/auth/contacts.readonly"]

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        install_manifest(&db, &registry, "p-google", manifest);
        let now = chrono::Utc::now().timestamp();
        OauthClientStore::new(&db)
            .upsert(&OauthClient {
                plugin_id: "p-google".into(),
                account_name: "controller".into(),
                provider: "google".into(),
                client_id: "cid".into(),
                client_secret: "secret".into(),
                redirect_uri: "http://x".into(),
                scopes_json: "[]".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        OauthTokenStore::new(&db)
            .upsert(&OauthTokens {
                plugin_id: "p-google".into(),
                account_name: "controller".into(),
                access_token: "ya29.tok".into(),
                refresh_token: Some("rt".into()),
                token_expires_at: now + 3600,
                scopes_granted: "[]".into(),
                account_email: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        // Enabled: token shows up.
        let host = PluginHost::new(db, registry.clone(), PathBuf::from("/tmp"));
        assert_eq!(host.oauth_tokens_for("p-google").len(), 1);
        // Disable: same database, but registry no longer caches the
        // accounts; token lookup is empty without a state_plugins
        // re-read.
        registry.disable("p-google");
        assert!(host.oauth_tokens_for("p-google").is_empty());
    }
}
