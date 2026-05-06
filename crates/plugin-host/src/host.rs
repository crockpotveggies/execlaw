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
    /// Phase B (2026-05-03) — when set, plugin install imports
    /// declared `[[skills]]` rows into the SkillStore (with
    /// `<plugin_id>/` namespace prepending) and plugin uninstall
    /// archives them. Empty disables both paths so existing tests
    /// and any caller that doesn't want skill side-effects continue
    /// to work unchanged. Set once at boot via [`Self::attach_skill_store`];
    /// `OnceLock` enforces single-init without needing to rebuild
    /// the `Inner` (which would clobber subprocesses / script
    /// plugins populated by an earlier hydrate call).
    skill_store: std::sync::OnceLock<Arc<execlaw_skills::SkillStore>>,
    /// Phase D.2 — sender clones are handed to each newly-spawned
    /// `SubprocessPlugin` so its reader can forward notifications
    /// (one-way `plugin → host` JSON-RPC messages with no `id`)
    /// into a single dispatcher task. Set by `attach_skill_store`.
    notification_tx: std::sync::OnceLock<
        tokio::sync::mpsc::UnboundedSender<crate::subprocess::PluginNotification>,
    >,
}

impl PluginHost {
    pub fn new(db: Database, registry: HookRegistry, stage_root: PathBuf) -> Self {
        Self::with_script_engine(
            db,
            registry,
            stage_root,
            execlaw_script::ScriptEngine::new(),
        )
    }

    /// Construct with a caller-provided script engine. Tests use
    /// this to inject a `ScriptEngine::with_loopback_allowed_for_tests()`
    /// so a mock HTTP server on `127.0.0.1` gets past the SSRF
    /// guard. Production paths call `new()` which always uses the
    /// production-default engine (SSRF on).
    pub fn with_script_engine(
        db: Database,
        registry: HookRegistry,
        stage_root: PathBuf,
        script_engine: execlaw_script::ScriptEngine,
    ) -> Self {
        Self {
            inner: Arc::new(PluginHostInner {
                db,
                registry,
                subprocesses: RwLock::new(BTreeMap::new()),
                script_plugins: RwLock::new(BTreeMap::new()),
                script_engine,
                stage_root,
                skill_store: std::sync::OnceLock::new(),
                notification_tx: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Attach a shared skill store. Idempotent on the same store
    /// (calls after the first are no-ops); subsequent calls with a
    /// DIFFERENT store are also no-ops because `OnceLock` only
    /// captures the first value. This lets boot order be flexible —
    /// `new()` → `hydrate()` → `attach_skill_store()` is fine and
    /// preserves anything hydrate populated.
    ///
    /// Side effect (Phase D.2, 2026-05-03): also spawns the
    /// notification dispatcher task that drains the
    /// plugin → host channel and routes `skill.register` /
    /// `skill.unregister` notifications to `SkillStore`. Subsequent
    /// `install()` calls hand each fresh `SubprocessPlugin` a sender
    /// clone so the dispatcher sees notifications from every plugin.
    pub fn attach_skill_store(&self, skill_store: Arc<execlaw_skills::SkillStore>) {
        if self.inner.skill_store.set(skill_store.clone()).is_err() {
            return; // already attached; dispatcher already running
        }
        // Spawn the notification dispatcher.
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::subprocess::PluginNotification>();
        // Stash the sender on the host so install() can clone it
        // when spawning each subprocess.
        let _ = self.inner.notification_tx.set(tx);
        let store = skill_store;
        tokio::spawn(async move {
            while let Some(n) = rx.recv().await {
                handle_plugin_notification(&store, n).await;
            }
        });
    }

    /// Borrow the attached skill store, if any. Used by tests to
    /// verify the install/uninstall side-effects landed.
    pub fn skill_store(&self) -> Option<&Arc<execlaw_skills::SkillStore>> {
        self.inner.skill_store.get()
    }

    /// Plug a host-capabilities surface into the script engine so
    /// `sidecar_url` / `ws_subscribe` / `host_route_inbound`
    /// bindings reach the host's concrete services. Call this once
    /// at boot AFTER `AppState` exists (which is what the caps
    /// implementation needs).
    ///
    /// Idempotent on the same caps; subsequent calls with a
    /// different caps are silent no-ops (OnceLock semantics inside
    /// the engine). Surfaced separately from `new()` because the
    /// engine factory has to be constructible BEFORE `AppState`
    /// exists — the chicken-and-egg between `AppState.plugin_host`
    /// and `AppStateHostCapabilities::new(state)`.
    pub fn attach_host_capabilities(
        &self,
        caps: execlaw_script::HostCapabilitiesArc,
    ) -> Result<(), execlaw_script::HostCapabilitiesArc> {
        self.inner.script_engine.set_host_capabilities(caps)
    }

    pub fn registry(&self) -> &HookRegistry {
        &self.inner.registry
    }

    /// Look up the live `ScriptPlugin` by id. Returns `None` for
    /// subprocess-tier plugins, plugins not yet loaded, or unknown
    /// ids. Surfaced for the admin-route dispatcher (and future
    /// plugin-introspection paths).
    pub async fn script_plugin(
        &self,
        plugin_id: &str,
    ) -> Option<execlaw_script::ScriptPlugin> {
        self.inner
            .script_plugins
            .read()
            .await
            .get(plugin_id)
            .cloned()
    }

    /// Fire the optional `on_enable()` Rhai lifecycle hook for
    /// every loaded script plugin. The cli boot path calls this
    /// AFTER the sidecar supervisor has been started — so a
    /// transport plugin's `on_enable` (typically `ws_subscribe`
    /// against the sidecar's WS endpoint) sees a live supervisor
    /// when it looks up `sidecar_url`.
    ///
    /// `wait_for_sidecars` should resolve once the supervisor has
    /// at least attempted to start every plugin's declared
    /// sidecars. The host doesn't wait for "all healthy" because a
    /// single broken sidecar would block every other plugin's
    /// lifecycle — instead, plugins whose sidecars are still
    /// crash-looping handle the `sidecar_url == None` case in
    /// their on_enable (typically: log + retry on next reload).
    ///
    /// Best-effort: a panicking or erroring on_enable logs at
    /// `warn` and the loop moves on to the next plugin.
    pub async fn fire_on_enable_for_all(&self) {
        let plugins: Vec<(String, execlaw_script::ScriptPlugin)> = self
            .inner
            .script_plugins
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (plugin_id, plugin) in plugins {
            if let Err(e) = plugin.call_on_enable().await {
                tracing::warn!(
                    target: "plugin_host::lifecycle",
                    plugin_id = %plugin_id,
                    error = %e,
                    "on_enable hook returned an error",
                );
            } else {
                tracing::debug!(
                    target: "plugin_host::lifecycle",
                    plugin_id = %plugin_id,
                    "on_enable fired",
                );
            }
        }
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

        // Step 1 — register hooks. Pass the stage path so any
        // declared `[[tools]].schema` files get loaded into the
        // registered tool's `schema_json` (the model needs the real
        // schema to call the tool with the right args).
        self.inner
            .registry
            .enable_with_stage(&manifest, Some(stage_path))
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
                    let plugin = match SubprocessPlugin::spawn(
                        spec,
                        self.inner.notification_tx.get().cloned(),
                    )
                    .await
                    {
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
                    let source_rel = runtime_source_or_err(runtime).inspect_err(|_| {
                        self.inner.registry.disable(&plugin_id);
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
                            return Err(PluginHostError::Spawn(format!("script load: {e}")));
                        }
                    };
                    self.inner
                        .script_plugins
                        .write()
                        .await
                        .insert(plugin_id.clone(), script);
                    // NOTE — on_enable is NOT fired here. Channel
                    // plugins that declare `[[services]]` need
                    // their sidecars healthy before on_enable runs
                    // (a `ws_subscribe` against a not-yet-started
                    // sidecar fails). The cli boot path fires
                    // on_enable via `fire_on_enable_for_all` once
                    // the sidecar supervisor reports the plugin's
                    // sidecars healthy. Plugins without sidecars
                    // get on_enable fired immediately by that pass.
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

        // Step 4 — Phase B: import any plugin-shipped skills. Best-
        // effort: failures here do NOT roll back the install. The
        // plugin's tools/transport are already wired and useful
        // even if a skill conflicted (e.g. with an admin-authored
        // skill of the same name). Operator sees a warning per
        // failure and can fix + reinstall — re-import on an
        // already-installed plugin appends new versions, so it's
        // safe to retry.
        if let Some(store) = self.inner.skill_store.get() {
            if !manifest.skills.is_empty() {
                let now_ms = now * 1000;
                let report = execlaw_skills::import_plugin_skills(
                    store,
                    &plugin_id,
                    &manifest.skills,
                    stage_path,
                    now_ms,
                );
                if !report.imported.is_empty() {
                    info!(
                        plugin_id,
                        imported = report.imported.len(),
                        "plugin skills imported"
                    );
                }
                for failure in &report.failed {
                    warn!(
                        plugin_id,
                        skill = %failure.plugin_local_name,
                        stored_name = ?failure.stored_name,
                        error = %failure.error,
                        "plugin skill import failed (install proceeded)"
                    );
                }
            }
        }

        info!(plugin_id, version, "plugin installed");
        Ok(row)
    }

    /// Replace an installed plugin with a newer version from
    /// `stage_path`. Used for graceful upgrades — the operator
    /// drops a v0.2 ZIP onto a v0.1 install and the per-plugin
    /// OAuth client config + granted tokens (which live in
    /// `state_oauth_clients` / `state_oauth_tokens`, NOT in
    /// `state_plugins`) survive untouched.
    ///
    /// Steps:
    ///
    /// 1. Parse the new manifest. Reject if its `plugin_id` doesn't
    ///    match an existing row — operators must use `install` for
    ///    a fresh plugin and `upgrade` only for an in-place version
    ///    bump. (Mismatched ids would silently drop the old install
    ///    and create a new one, which is a footgun.)
    /// 2. Tear down the old runtime: disable hooks, drop the
    ///    subprocess / script engine, remove the staged dir.
    /// 3. Delete the old `state_plugins` row.
    /// 4. Run the install pipeline against the new stage_path.
    ///
    /// Failure semantics: if step 4's hook registration or
    /// subprocess spawn fails, the operator is left in
    /// "uninstalled" state (their OAuth rows still survive in the
    /// other tables). This is acceptable — the new ZIP is broken;
    /// the operator can either fix it and retry, re-upload the
    /// old version, or reconnect their OAuth account if they want
    /// to start fresh. Restoring the old runtime mid-failure adds
    /// a lot of edge-case surface for a path that should be rare.
    pub async fn upgrade(&self, stage_path: &Path) -> Result<PluginRow, PluginHostError> {
        let manifest_path = stage_path.join("plugin.toml");
        let manifest_toml = std::fs::read_to_string(&manifest_path)
            .map_err(|e| PluginHostError::Manifest(format!("read plugin.toml: {e}")))?;
        let manifest = PluginManifest::parse(&manifest_toml)
            .map_err(|e| PluginHostError::Manifest(e.to_string()))?;
        let new_id = manifest.plugin.id.clone();

        let existing = self
            .get_row(&new_id)?
            .ok_or_else(|| PluginHostError::NotInstalled(new_id.clone()))?;

        info!(
            plugin_id = %new_id,
            from = %existing.version,
            to = %manifest.plugin.version,
            "upgrading plugin",
        );

        // Tear down old runtime + DB row in the same shape as
        // `uninstall`, except we keep the OAuth rows + we drive
        // the new install ourselves below.
        self.inner.registry.disable(&new_id);
        if let Some(plugin) = self.inner.subprocesses.write().await.remove(&new_id) {
            plugin.shutdown().await;
        }
        let _ = self.inner.script_plugins.write().await.remove(&new_id);
        self.delete_row(&new_id)?;
        // Best-effort remove the OLD staged directory. If it
        // happens to be the SAME path as the new one (operator
        // re-extracted in place), skip — we'd nuke the source.
        let new_stage_canon = stage_path.canonicalize().ok();
        let old_stage_canon = std::path::Path::new(&existing.stage_path)
            .canonicalize()
            .ok();
        let same_dir = matches!((&new_stage_canon, &old_stage_canon), (Some(a), Some(b)) if a == b);
        if !same_dir {
            if let Err(e) = std::fs::remove_dir_all(&existing.stage_path) {
                warn!(
                    plugin_id = %new_id,
                    path = %existing.stage_path,
                    error = %e,
                    "failed to remove old staged dir during upgrade",
                );
            }
        }

        // Now run the standard install pipeline against the new
        // stage. This will register hooks + spawn runtime + insert
        // a fresh state_plugins row. If it fails the operator is
        // in uninstalled state (see method-level docs).
        self.install(stage_path).await
    }

    /// Uninstall: disable hooks, kill subprocess, archive plugin-
    /// shipped skills (Phase B), remove DB row + staged directory.
    /// Idempotent — missing plugin returns `NotInstalled`.
    pub async fn uninstall(&self, plugin_id: &str) -> Result<(), PluginHostError> {
        let row = self
            .get_row(plugin_id)?
            .ok_or_else(|| PluginHostError::NotInstalled(plugin_id.to_owned()))?;

        self.inner.registry.disable(plugin_id);

        if let Some(plugin) = self.inner.subprocesses.write().await.remove(plugin_id) {
            plugin.shutdown().await;
        }
        let _ = self.inner.script_plugins.write().await.remove(plugin_id);

        // Phase B (2026-05-03) — archive skills owned by this
        // plugin BEFORE deleting the install row so the
        // `state_skill_invocations.skill_id` foreign-key path stays
        // valid for forensic queries on archived skills. Best-effort:
        // a failure here is logged but doesn't block the uninstall.
        if let Some(store) = self.inner.skill_store.get() {
            let now_ms = chrono::Utc::now().timestamp() * 1000;
            match store.archive_for_plugin(plugin_id, now_ms) {
                Ok(archived) if !archived.is_empty() => {
                    info!(
                        plugin_id,
                        count = archived.len(),
                        "archived plugin-shipped skills"
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(
                    plugin_id,
                    error = %e,
                    "failed to archive plugin skills (uninstall continued)"
                ),
            }
        }

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
        if let Some(plugin) = self.inner.subprocesses.write().await.remove(plugin_id) {
            plugin.shutdown().await;
        }
        // Drop the script plugin (if any). No subprocess to kill —
        // the rhai::Engine is just memory.
        let _ = self.inner.script_plugins.write().await.remove(plugin_id);
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
            .enable_with_stage(&manifest, Some(std::path::Path::new(&row.stage_path)))
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
                        executable: resolve_executable(&stage, runtime_executable_or_err(runtime)?),
                        args: runtime.args.clone(),
                        cwd: Some(stage),
                    };
                    let plugin =
                        SubprocessPlugin::spawn(spec, self.inner.notification_tx.get().cloned())
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

        // Fire on_enable for script plugins so a fresh enable / a
        // post-install upgrade gets the same lifecycle treatment as
        // a boot-time hydrate. on_enable handlers are expected to be
        // robust to "sidecar not yet up" (use `sidecar_url_blocking`
        // or equivalent polling) so this is safe to call regardless
        // of supervisor state. Best-effort — a panicking or erroring
        // hook is logged but doesn't fail enable().
        let plugin = self
            .inner
            .script_plugins
            .read()
            .await
            .get(plugin_id)
            .cloned();
        if let Some(plugin) = plugin {
            // Spawn so the HTTP request returns immediately;
            // on_enable can poll for up to a few minutes.
            let pid = plugin_id.to_owned();
            tokio::spawn(async move {
                if let Err(e) = plugin.call_on_enable().await {
                    warn!(plugin_id = %pid, error = %e, "on_enable hook returned an error");
                } else {
                    debug!(plugin_id = %pid, "on_enable fired (post-enable)");
                }
            });
        }
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
            if let Err(e) = self
                .inner
                .registry
                .enable_with_stage(&manifest, Some(std::path::Path::new(&row.stage_path)))
            {
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
                            match SubprocessPlugin::spawn(
                                spec,
                                self.inner.notification_tx.get().cloned(),
                            )
                            .await
                            {
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
                                    // on_enable is fired by the boot
                                    // path's `fire_on_enable_for_all`
                                    // after the supervisor reports
                                    // sidecars healthy.
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
    pub async fn resolve_identity(&self, transport: &str, handle: &str) -> Vec<serde_json::Value> {
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
    ///
    /// **Trust-floor enforcement**: if the manifest declares
    /// `[[tools]].trust_floor`, the `caller_trust` rank must be at or
    /// above that floor. This is the analogue of selfhosted-claw's
    /// `controllerOnly: true` knob, but generalised so a plugin can
    /// pin a tool at e.g. `KnownTrusted`. `caller_trust = None` means
    /// the caller skipped the gate (legacy paths) — preserve the old
    /// behaviour and only enforce capabilities. New call sites should
    /// always pass `Some(_)`.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        caller_caps: &[&str],
        caller_trust: Option<&str>,
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

        // Trust-floor check. We compare ranks rather than exact-match
        // so e.g. `Controller` (rank 5) satisfies a `KnownTrusted`
        // (rank 3) floor. Manifest validation already rejected
        // unknown floor strings, but we re-validate defensively here:
        // a stale registry entry whose plugin shipped before this
        // field existed would have `trust_floor = None` and skip the
        // check entirely.
        if let Some(floor) = registered.trust_floor.as_deref() {
            let floor_rank = trust_rank(floor);
            let caller_rank = caller_trust.map(trust_rank).unwrap_or(0);
            if caller_rank < floor_rank {
                let caller_label = caller_trust.unwrap_or("<none>");
                return Err(format!(
                    "tool '{tool_name}' requires trust >= {floor} but caller is {caller_label}"
                ));
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
        self.inner
            .db
            .with_conn(|c| {
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
            })
            .map_err(PluginHostError::Db)
    }

    pub fn get_row(&self, plugin_id: &str) -> Result<Option<PluginRow>, PluginHostError> {
        self.inner
            .db
            .with_conn(|c| {
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
            })
            .map_err(PluginHostError::Db)
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
        self.inner
            .db
            .with_conn(|c| {
                c.execute(
                    "UPDATE state_plugins SET enabled = ?1, updated_at = ?2 WHERE plugin_id = ?3",
                    params![row.enabled as i64, row.updated_at, row.plugin_id],
                )?;
                Ok(())
            })
            .map_err(PluginHostError::Db)
    }

    fn delete_row(&self, plugin_id: &str) -> Result<(), PluginHostError> {
        self.inner
            .db
            .with_conn(|c| {
                c.execute(
                    "DELETE FROM state_plugins WHERE plugin_id = ?1",
                    params![plugin_id],
                )?;
                Ok(())
            })
            .map_err(PluginHostError::Db)
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
/// Map a trust-level string to its rank — higher = more trusted.
/// Mirrors `execlaw_policy::trust::TrustLevel::rank` exactly so the
/// `caller_trust` rank passed through from the dispatch layer compares
/// correctly. Kept as a private helper so `plugin-host` doesn't pull
/// `execlaw-policy` into its dep graph just for one enum.
///
/// Unknown strings (including "<none>" and the empty string) map to
/// 0 — i.e. strictly below `Blocked`. That makes the trust-floor
/// comparison conservative: a typo or stale call site can't
/// accidentally let a tool through.
fn trust_rank(s: &str) -> u8 {
    match s {
        "Controller" => 5,
        "Delegated" => 4,
        "KnownTrusted" => 3,
        "KnownLimited" => 2,
        "UnknownPending" => 1,
        "Blocked" => 0,
        _ => 0,
    }
}

fn oauth_map_from_params(params: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
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

/// Phase D.2 — handle one plugin → host notification. Today we only
/// understand `skill.register` and `skill.unregister`; future
/// methods land here as additional match arms.
async fn handle_plugin_notification(
    skill_store: &Arc<execlaw_skills::SkillStore>,
    n: crate::subprocess::PluginNotification,
) {
    let plugin_id = n.plugin_id.clone();
    match n.method.as_str() {
        "skill.register" => {
            #[derive(serde::Deserialize)]
            struct RegisterParams {
                name: String,
                description: String,
                body_md: String,
                #[serde(default)]
                tags: Vec<String>,
            }
            let parsed: Result<RegisterParams, _> = serde_json::from_value(n.params.clone());
            let p = match parsed {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        plugin_id,
                        error = %e,
                        "skill.register: invalid params"
                    );
                    return;
                }
            };
            // Build a `SkillDecl` and route through the existing
            // import_plugin_skills path so the namespace prefix +
            // sanitization rules are identical to ZIP-shipped skills.
            // The `entry` field is a synthetic in-memory body; we
            // bypass the file read by writing the body directly.
            let stored_name = execlaw_skills::namespaced_name(&plugin_id, &p.name);
            let frontmatter = serde_json::json!({
                "name": stored_name,
                "description": p.description,
                "tags": p.tags,
                "registered_at_runtime": true,
            })
            .to_string();
            let now_ms = chrono::Utc::now().timestamp() * 1000;
            let new = execlaw_skills::NewSkill {
                name: stored_name.clone(),
                source: format!("plugin:{plugin_id}"),
                registration_kind: execlaw_skills::RegistrationKind::Registered,
                owning_plugin_id: Some(plugin_id.clone()),
                initial_version: execlaw_skills::NewSkillVersion {
                    description: p.description,
                    body_md: p.body_md,
                    frontmatter_json: frontmatter,
                    authored_by: format!("plugin:{plugin_id}"),
                    promotion_notes: None,
                },
                resources: vec![],
            };
            match skill_store.import_shipped(new, now_ms) {
                Ok(_) => info!(
                    plugin_id,
                    skill = %stored_name,
                    "plugin registered skill at runtime"
                ),
                Err(e) => warn!(
                    plugin_id,
                    skill = %stored_name,
                    error = %e,
                    "skill.register failed"
                ),
            }
        }
        "skill.unregister" => {
            #[derive(serde::Deserialize)]
            struct UnregisterParams {
                name: String,
            }
            let parsed: Result<UnregisterParams, _> = serde_json::from_value(n.params.clone());
            let p = match parsed {
                Ok(p) => p,
                Err(e) => {
                    warn!(plugin_id, error = %e, "skill.unregister: invalid params");
                    return;
                }
            };
            let stored_name = execlaw_skills::namespaced_name(&plugin_id, &p.name);
            // Verify the skill is owned by THIS plugin before
            // archiving — defense against a misbehaving plugin
            // trying to unregister someone else's skill.
            let owner_check = skill_store.get(&stored_name);
            match owner_check {
                Ok(Some(s)) if s.owning_plugin_id.as_deref() == Some(&plugin_id) => {
                    let now_ms = chrono::Utc::now().timestamp() * 1000;
                    if let Err(e) = skill_store.archive(&stored_name, now_ms) {
                        warn!(
                            plugin_id,
                            skill = %stored_name,
                            error = %e,
                            "skill.unregister: archive failed"
                        );
                    } else {
                        info!(
                            plugin_id,
                            skill = %stored_name,
                            "plugin unregistered skill at runtime"
                        );
                    }
                }
                Ok(Some(s)) => warn!(
                    plugin_id,
                    skill = %stored_name,
                    actual_owner = ?s.owning_plugin_id,
                    "skill.unregister: refusing — not owned by this plugin"
                ),
                Ok(None) => debug!(
                    plugin_id,
                    skill = %stored_name,
                    "skill.unregister: target does not exist; ignoring"
                ),
                Err(e) => warn!(
                    plugin_id,
                    skill = %stored_name,
                    error = %e,
                    "skill.unregister: lookup failed"
                ),
            }
        }
        other => debug!(
            plugin_id,
            method = %other,
            "plugin notification with unknown method; ignored"
        ),
    }
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
        assert_eq!(map.get("team").and_then(|v| v.as_str()), Some("ya29.team"),);
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

    // ---- upgrade ---------------------------------------------------

    /// Build a real on-disk staged plugin dir containing
    /// `plugin.toml` + `main.rhai`. Returns the TempDir (caller
    /// must keep it alive) plus the stage path inside it.
    fn stage_script_plugin(
        plugin_id: &str,
        version: &str,
        scope: &str,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join(format!("{plugin_id}-{version}"));
        std::fs::create_dir_all(&stage).unwrap();
        let manifest = format!(
            r#"
[plugin]
id = "{plugin_id}"
name = "Upgrade Test"
version = "{version}"
description = "test"
author = "a"
license = "x"

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["{scope}"]

[runtime]
tier = "script"
source = "main.rhai"
"#
        );
        std::fs::write(stage.join("plugin.toml"), manifest).unwrap();
        // Minimal Rhai source — declare the entry point the host
        // will compile but never call in these tests.
        std::fs::write(
            stage.join("main.rhai"),
            "fn tool_call(name, args, oauth) { #{} }\n",
        )
        .unwrap();
        (dir, stage)
    }

    /// Phase B (2026-05-03) — stages a script-tier plugin that
    /// declares two `[[skills]]` rows + ships their body files.
    /// Returns the temp dir keep-alive guard, the stage path the
    /// caller hands to `host.install`, and the plugin id.
    fn stage_plugin_with_skills(plugin_id: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join(format!("{plugin_id}-0.1.0"));
        std::fs::create_dir_all(stage.join("skills")).unwrap();
        let manifest = format!(
            r#"
[plugin]
id = "{plugin_id}"
name = "Skills Test"
version = "0.1.0"
description = "test"
author = "a"
license = "x"

[[skills]]
name = "Query Builder"
description = "Use to construct SQL queries against Postgres."
entry = "skills/query-builder.md"
tags = ["db", "sql"]

[[skills]]
name = "migrate"
description = "Use to plan and run database migrations."
entry = "skills/migrate.md"

[runtime]
tier = "script"
source = "main.rhai"
"#
        );
        std::fs::write(stage.join("plugin.toml"), manifest).unwrap();
        std::fs::write(
            stage.join("main.rhai"),
            "fn tool_call(name, args, oauth) { #{} }\n",
        )
        .unwrap();
        std::fs::write(
            stage.join("skills/query-builder.md"),
            "# Query Builder\n\nUse SELECT carefully.\n",
        )
        .unwrap();
        std::fs::write(
            stage.join("skills/migrate.md"),
            "# Migrations\n\nAlways back up first.\n",
        )
        .unwrap();
        (dir, stage)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_imports_plugin_skills_with_namespaced_names() {
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db.clone(), registry, stage_root.path().to_path_buf());
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        host.attach_skill_store(store.clone());

        let (_keep, stage) = stage_plugin_with_skills("postgres-toolkit");
        host.install(&stage).await.unwrap();

        let names = store.list_for_plugin("postgres-toolkit").unwrap();
        assert_eq!(
            names,
            vec![
                "postgres-toolkit/migrate".to_string(),
                "postgres-toolkit/query-builder".to_string(),
            ],
            "both shipped skills must land with namespaced names"
        );

        // The skill's content + frontmatter is intact.
        let view = store
            .view("postgres-toolkit/query-builder")
            .unwrap()
            .unwrap();
        assert!(view.body_md.contains("SELECT carefully"));
        assert_eq!(
            view.description,
            "Use to construct SQL queries against Postgres."
        );
        let fm: serde_json::Value = serde_json::from_str(&view.frontmatter_json).unwrap();
        assert_eq!(fm["tags"][0], "db");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_with_no_attached_store_is_a_no_op_for_skills() {
        // Without a skill store attached, install() must succeed and
        // simply ignore the [[skills]] blocks. Existing tests +
        // callers that don't care about skills are unaffected.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db.clone(), registry, stage_root.path().to_path_buf());
        // No attach_skill_store call.

        let (_keep, stage) = stage_plugin_with_skills("p-noskills");
        host.install(&stage).await.unwrap();
        // No state_skills rows landed because no store was attached.
        let count: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM state_skills", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uninstall_archives_plugin_shipped_skills() {
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db.clone(), registry, stage_root.path().to_path_buf());
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        host.attach_skill_store(store.clone());

        let (_keep, stage) = stage_plugin_with_skills("ephemeral");
        host.install(&stage).await.unwrap();
        assert_eq!(store.list_for_plugin("ephemeral").unwrap().len(), 2);

        host.uninstall("ephemeral").await.unwrap();
        // Active list is empty…
        assert!(store.list_for_plugin("ephemeral").unwrap().is_empty());
        // …but the rows still exist in archived state for forensic
        // queries (state_skill_invocations FKs stay valid).
        let archived: i64 = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM state_skills
                     WHERE owning_plugin_id = ?1 AND state = 'archived'",
                    params!["ephemeral"],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(archived, 2);
    }

    /// Phase D.2 — `handle_plugin_notification` is the host's
    /// single dispatch point for plugin → host notifications.
    /// Unit-tested directly because driving a subprocess through
    /// the full RPC pipeline is platform-flaky on Windows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_plugin_notification_register_lands_a_namespaced_skill() {
        let db = fresh_db();
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "postgres-toolkit".into(),
                method: "skill.register".into(),
                params: serde_json::json!({
                    "name": "Query Builder",
                    "description": "Use to build SELECT queries.",
                    "body_md": "1. SELECT carefully.\n2. LIMIT.",
                    "tags": ["db", "sql"]
                }),
            },
        )
        .await;
        let names = store.list_for_plugin("postgres-toolkit").unwrap();
        assert_eq!(names, vec!["postgres-toolkit/query-builder"]);
        let g = store
            .get("postgres-toolkit/query-builder")
            .unwrap()
            .unwrap();
        assert_eq!(
            g.registration_kind,
            execlaw_skills::RegistrationKind::Registered
        );
        assert!(g.current_version.body_md.contains("SELECT carefully"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_plugin_notification_unregister_archives_only_own_skill() {
        let db = fresh_db();
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        // Plugin "alpha" registers a skill.
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "alpha".into(),
                method: "skill.register".into(),
                params: serde_json::json!({
                    "name": "x",
                    "description": "alpha's",
                    "body_md": "body",
                }),
            },
        )
        .await;
        assert_eq!(store.list_for_plugin("alpha").unwrap(), vec!["alpha/x"]);

        // Plugin "beta" tries to unregister alpha's skill — must be refused.
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "beta".into(),
                method: "skill.unregister".into(),
                params: serde_json::json!({"name": "x"}),
            },
        )
        .await;
        // Note: namespaced names differ — alpha/x vs beta/x.
        // beta's unregister target (beta/x) doesn't exist, so it's a no-op.
        // alpha/x is unchanged.
        assert_eq!(store.list_for_plugin("alpha").unwrap(), vec!["alpha/x"]);

        // Now alpha legitimately unregisters its own.
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "alpha".into(),
                method: "skill.unregister".into(),
                params: serde_json::json!({"name": "x"}),
            },
        )
        .await;
        assert!(store.list_for_plugin("alpha").unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_plugin_notification_invalid_params_is_logged_not_panicked() {
        let db = fresh_db();
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        // Garbage params.
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "p".into(),
                method: "skill.register".into(),
                params: serde_json::json!({"wrong": "shape"}),
            },
        )
        .await;
        // Unknown method.
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "p".into(),
                method: "skill.unknown".into(),
                params: serde_json::Value::Null,
            },
        )
        .await;
        // No state changes — assertion via row count.
        let count: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM state_skills", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_plugin_notification_register_credential_is_rejected_by_scanner() {
        let db = fresh_db();
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        super::handle_plugin_notification(
            &store,
            crate::subprocess::PluginNotification {
                plugin_id: "p".into(),
                method: "skill.register".into(),
                params: serde_json::json!({
                    "name": "leaky",
                    "description": "leaks",
                    "body_md": "use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to call",
                }),
            },
        )
        .await;
        // Scanner blocks; no row landed.
        assert!(store.list_for_plugin("p").unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_continues_when_one_skill_conflicts_with_admin_authored() {
        // Admin pre-authors a skill whose namespaced name will
        // collide with one of the plugin's skills. The install
        // must still succeed; the conflicting skill is logged as
        // failed but the other one lands. This is the partial-
        // success contract from the import design.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db.clone(), registry, stage_root.path().to_path_buf());
        let store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
        host.attach_skill_store(store.clone());

        // Admin owns `pg/query-builder` first, before the plugin
        // ships its own.
        store
            .create(
                execlaw_skills::NewSkill {
                    name: "pg/query-builder".into(),
                    source: "admin:Controller".into(),
                    registration_kind: execlaw_skills::RegistrationKind::Authored,
                    owning_plugin_id: None,
                    initial_version: execlaw_skills::NewSkillVersion {
                        description: "admin's version".into(),
                        body_md: "by admin".into(),
                        frontmatter_json: "{}".into(),
                        authored_by: "admin:Controller".into(),
                        promotion_notes: None,
                    },
                    resources: vec![],
                },
                execlaw_skills::Strictness::Strict,
                100,
            )
            .unwrap();

        let (_keep, stage) = stage_plugin_with_skills("pg");
        // Install proceeds: `pg/migrate` lands, `pg/query-builder`
        // is rejected as a conflict (admin's version already owns
        // that name).
        host.install(&stage).await.unwrap();

        let names = store.list_for_plugin("pg").unwrap();
        assert_eq!(names, vec!["pg/migrate".to_string()]);

        // Admin's skill is unchanged: still admin-authored, still
        // their original body.
        let admins = store.get("pg/query-builder").unwrap().unwrap();
        assert_eq!(
            admins.registration_kind,
            execlaw_skills::RegistrationKind::Authored
        );
        assert_eq!(admins.current_version.body_md, "by admin");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrade_replaces_version_and_preserves_oauth_client_and_tokens() {
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db.clone(), registry, stage_root.path().to_path_buf());

        let (_v1_keep, v1_stage) = stage_script_plugin(
            "test-google",
            "0.1.0",
            "https://www.googleapis.com/auth/calendar.readonly",
        );
        let row = host.install(&v1_stage).await.unwrap();
        assert_eq!(row.version, "0.1.0");

        // Operator connected the OAuth account on v0.1: pretend
        // they entered a client + we cached a token.
        let now = chrono::Utc::now().timestamp();
        OauthClientStore::new(&db)
            .upsert(&OauthClient {
                plugin_id: "test-google".into(),
                account_name: "controller".into(),
                provider: "google".into(),
                client_id: "client-xyz".into(),
                client_secret: "secret-abc".into(),
                redirect_uri: "http://localhost/cb".into(),
                scopes_json: r#"["scope-a"]"#.into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        OauthTokenStore::new(&db)
            .upsert(&OauthTokens {
                plugin_id: "test-google".into(),
                account_name: "controller".into(),
                access_token: "ya29.preserve-me".into(),
                refresh_token: Some("refresh-preserve".into()),
                token_expires_at: now + 3600,
                scopes_granted: r#"["scope-a"]"#.into(),
                account_email: Some("op@example.com".into()),
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        // Upgrade to v0.2 with an expanded scope (mirrors the
        // calendar-plugin readonly → events bump that motivated
        // this feature).
        let (_v2_keep, v2_stage) = stage_script_plugin(
            "test-google",
            "0.2.0",
            "https://www.googleapis.com/auth/calendar.events",
        );
        let row = host.upgrade(&v2_stage).await.unwrap();
        assert_eq!(row.version, "0.2.0");

        // OAuth client survived intact — same id + secret + scopes.
        let client = OauthClientStore::new(&db)
            .get("test-google", "controller")
            .unwrap()
            .expect("oauth client must survive upgrade");
        assert_eq!(client.client_id, "client-xyz");
        assert_eq!(client.client_secret, "secret-abc");

        // Token survived — operator doesn't need to re-authenticate
        // for the basic case (scope-narrowed callers WILL fail on
        // first use; that's surfaced separately by the provider).
        let token = OauthTokenStore::new(&db)
            .get("test-google", "controller")
            .unwrap()
            .expect("oauth token must survive upgrade");
        assert_eq!(token.access_token, "ya29.preserve-me");
        assert_eq!(token.refresh_token.as_deref(), Some("refresh-preserve"));
        assert_eq!(token.account_email.as_deref(), Some("op@example.com"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrade_rejects_when_no_existing_install() {
        // Operators have to use install (or `if_existing=upgrade`
        // which falls through to install). Calling upgrade
        // directly on a clean DB is a programming error.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db, registry, stage_root.path().to_path_buf());

        let (_keep, stage) =
            stage_script_plugin("ghost", "0.1.0", "https://www.googleapis.com/auth/x");
        let err = host.upgrade(&stage).await.unwrap_err();
        assert!(
            matches!(err, PluginHostError::NotInstalled(ref id) if id == "ghost"),
            "got: {err:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrade_writes_new_version_string_to_state_plugins() {
        let db = fresh_db();
        let registry = HookRegistry::new();
        let stage_root = tempfile::tempdir().unwrap();
        let host = PluginHost::new(db.clone(), registry, stage_root.path().to_path_buf());

        let (_v1, v1_stage) =
            stage_script_plugin("v-test", "0.1.0", "https://www.googleapis.com/auth/x");
        host.install(&v1_stage).await.unwrap();
        let (_v2, v2_stage) =
            stage_script_plugin("v-test", "1.4.7", "https://www.googleapis.com/auth/x");
        host.upgrade(&v2_stage).await.unwrap();

        let row = host
            .get_row("v-test")
            .unwrap()
            .expect("row must exist post-upgrade");
        assert_eq!(row.version, "1.4.7");
    }

    // === trust_floor enforcement ===

    #[test]
    fn trust_rank_orders_match_policy_crate() {
        // Spot-check the rank ordering — must match
        // execlaw_policy::trust::TrustLevel::rank exactly so a
        // Controller satisfies a KnownTrusted floor and a
        // KnownLimited fails it.
        assert!(trust_rank("Controller") > trust_rank("KnownTrusted"));
        assert!(trust_rank("KnownTrusted") > trust_rank("KnownLimited"));
        assert!(trust_rank("KnownLimited") > trust_rank("UnknownPending"));
        assert!(trust_rank("UnknownPending") > trust_rank("Blocked"));
        assert_eq!(trust_rank("Controller"), 5);
        assert_eq!(trust_rank("Blocked"), 0);
        // Unknown / typo => 0 (strictly below Blocked) so a stale
        // call site can't accidentally bypass enforcement.
        assert_eq!(trust_rank("admin"), 0);
        assert_eq!(trust_rank(""), 0);
    }

    #[tokio::test]
    async fn call_tool_rejects_caller_below_trust_floor() {
        // Plugin declares a Controller-floor tool. A KnownLimited
        // caller must be rejected BEFORE the dispatcher tries to
        // load any plugin runtime. Verifying the error message also
        // pins the operator-facing string we surface in tool-result
        // payloads.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let manifest = r#"
[plugin]
id = "signal"
name = "Signal"
version = "0.1.0"

[[tools]]
name = "signal.send_message"
description = "Send a Signal message."
trust_floor = "Controller"

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        install_manifest(&db, &registry, "signal", manifest);
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));

        let err = host
            .call_tool(
                "signal.send_message",
                serde_json::json!({"to": "alice", "text": "hi"}),
                &[],
                Some("KnownLimited"),
            )
            .await
            .expect_err("KnownLimited must not pass a Controller floor");
        assert!(
            err.contains("trust >= Controller"),
            "error should name the floor — got {err:?}",
        );
        assert!(
            err.contains("KnownLimited"),
            "error should name the actual caller — got {err:?}",
        );
    }

    #[tokio::test]
    async fn call_tool_passes_trust_floor_when_caller_is_above() {
        // A Controller caller satisfies a Controller floor — the
        // call must proceed past the gate. It still fails because
        // the manifest's `subprocess` runtime points at a
        // nonexistent binary, but the failure mode is "no runtime
        // loaded", not "trust violation". That's the assertion: we
        // pass the gate.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let manifest = r#"
[plugin]
id = "signal"
name = "Signal"
version = "0.1.0"

[[tools]]
name = "signal.send_message"
description = "Send a Signal message."
trust_floor = "Controller"

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        install_manifest(&db, &registry, "signal", manifest);
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));

        let err = host
            .call_tool(
                "signal.send_message",
                serde_json::json!({}),
                &["*"],
                Some("Controller"),
            )
            .await
            .expect_err("no runtime loaded => Err, but NOT a trust error");
        assert!(
            !err.contains("trust"),
            "must not be a trust error — got {err:?}",
        );
        assert!(
            err.contains("no runtime is loaded") || err.contains("not registered"),
            "expected 'no runtime' shape — got {err:?}",
        );
    }

    #[tokio::test]
    async fn call_tool_with_no_floor_accepts_any_caller_trust() {
        // Tools that omit `trust_floor` keep the legacy behaviour:
        // capability gate only. A KnownLimited caller still passes
        // when no floor is declared.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let manifest = r#"
[plugin]
id = "weather"
name = "Weather"
version = "0.1.0"

[[tools]]
name = "weather.lookup"
description = "Look up the weather."

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        install_manifest(&db, &registry, "weather", manifest);
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));

        let err = host
            .call_tool(
                "weather.lookup",
                serde_json::json!({}),
                &[],
                Some("KnownLimited"),
            )
            .await
            .expect_err("no runtime loaded => Err, but NOT a trust error");
        assert!(
            !err.contains("trust"),
            "no trust_floor declared — must not block on trust",
        );
    }

    #[tokio::test]
    async fn call_tool_with_no_caller_trust_still_blocks_floor() {
        // Defensive: a legacy call site that passes `caller_trust =
        // None` must NOT be allowed to invoke a floor-protected
        // tool. The conservative read is "unknown caller", which
        // ranks 0 → strictly below every declared floor.
        let db = fresh_db();
        let registry = HookRegistry::new();
        let manifest = r#"
[plugin]
id = "signal"
name = "Signal"
version = "0.1.0"

[[tools]]
name = "signal.send_message"
description = "Send a Signal message."
trust_floor = "KnownTrusted"

[runtime]
tier = "subprocess"
executable = "./bin"
"#;
        install_manifest(&db, &registry, "signal", manifest);
        let host = PluginHost::new(db, registry, PathBuf::from("/tmp"));

        let err = host
            .call_tool("signal.send_message", serde_json::json!({}), &["*"], None)
            .await
            .expect_err("no caller_trust => below any floor");
        assert!(
            err.contains("trust >= KnownTrusted"),
            "expected trust violation — got {err:?}",
        );
    }
}
