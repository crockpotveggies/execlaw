//! `PythonSandboxService` — runtime glue that ties the gateway
//! client, kernel pool, and output watcher together for the
//! python-sandbox plugin's tool dispatchers.
//!
//! Constructed once at boot, after the kernel-gateway sidecar
//! reports healthy. Held as `Arc<PythonSandboxService>` by each of
//! the four `ToolImpl`s; cheap to clone, internally Sync.
//!
//! Lifecycle:
//!   * `new(...)` constructs the client + pool, starts the output
//!     watcher with a callback that publishes new files as artifacts,
//!     starts the idle-eviction worker.
//!   * Drop tears everything down: watcher's OS thread exits, timer
//!     tasks abort, kernels on the gateway are NOT explicitly
//!     deleted (they'll be culled by the gateway's own idle policy
//!     after restart — same recovery path as a host crash).

use crate::cards::{close_card_and_broadcast, open_card_and_broadcast};
use crate::events::EventBus;
use crate::python_sandbox::client::{GatewayClient, GatewayError};
use crate::python_sandbox::hydration::{
    AttachmentToHydrate, HydrateOpts, HydrationError, hydrate_uploads,
};
use crate::python_sandbox::kernel_pool::KernelPool;
use crate::python_sandbox::output_watcher::{DEFAULT_DEBOUNCE, OutputCreated, OutputWatcher};
use execlaw_core::Database;
use execlaw_core::attachments::AttachmentStore;
use execlaw_core::cards::{CardAction, CardClosedPayload, CardKind, CardOpenedPayload, CardState};
use execlaw_core::ids::ConversationId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

pub const PLUGIN_ID: &str = "python-sandbox";

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("gateway client setup failed: {0}")]
    Gateway(#[from] GatewayError),
    #[error("output watcher setup failed: {0}")]
    Watcher(#[from] crate::python_sandbox::output_watcher::WatchError),
}

/// Holds the long-lived per-host machinery for the python_sandbox
/// plugin. One instance per running server.
pub struct PythonSandboxService {
    pool: KernelPool,
    work_root: PathBuf,
    artifacts_root: PathBuf,
    db: Database,
    /// Per-conversation hydration watermark — the
    /// `MAX(received_at)` we last observed in `state_attachments`
    /// for that conversation. `hydrate_if_needed` re-queries the
    /// current max on each call; a mismatch means a new attachment
    /// landed (web composer, transport-inbound, or
    /// `web_fetch(as_attachment: true)`) and we need to re-walk.
    /// A match means the conversation's attachment set is unchanged
    /// since the last hydration and we can skip the dir walk.
    ///
    /// Pre-2026-05-25 this was a `HashSet<ConversationId>` — once a
    /// conversation was hydrated, the cache short-circuited every
    /// subsequent execute. Broke `web_fetch(as_attachment: true)`
    /// because the freshly-written attachment was invisible to the
    /// next execute. The watermark variant self-heals on any new
    /// attachment from any code path.
    ///
    /// `i64` value because `state_attachments.received_at` is a
    /// unix-seconds epoch stored as INTEGER.
    hydrated_convos: Arc<Mutex<HashMap<ConversationId, i64>>>,
    _eviction_handle: tokio::task::JoinHandle<()>,
    // OutputWatcher dropped on Drop tears down its OS thread + tokio
    // timer. Kept named to make the lifetime obvious to readers.
    _watcher: OutputWatcher,
}

/// Error from the hydrate-on-first-execute path. Distinct from
/// HydrationError so callers don't have to know about the DB-side.
#[derive(Debug, Error)]
pub enum HydrateOnExecuteError {
    #[error("list state_attachments for {convo}: {source}")]
    Db {
        convo: String,
        #[source]
        source: execlaw_core::DbError,
    },
    #[error("hydrate uploads for {convo}: {source}")]
    Hydrate {
        convo: String,
        #[source]
        source: HydrationError,
    },
}

/// Operator-tunable knobs the SPA panel persists via the
/// plugin-settings vault. Read once at boot by `wire_python_sandbox`
/// and threaded into `PythonSandboxService::new_with_settings`. All
/// fields are `Option<T>` with `None` falling back to the constants
/// defined alongside the consumer (`DEFAULT_IDLE_TIMEOUT` in
/// kernel_pool, `MAX_OUTPUT_BYTES` in client) — so a fresh install
/// with no settings written behaves identically to before this
/// type existed.
#[derive(Debug, Default, Clone)]
pub struct PythonSandboxSettings {
    /// How long an inactive kernel stays alive before the pool
    /// evicts it. Lower = less idle memory; higher = faster repeats
    /// for the same conversation. SPA key:
    /// `kernel_idle_timeout_seconds`.
    pub idle_timeout: Option<std::time::Duration>,
    /// Hard cap on a single `python.execute` output buffer.
    /// Exceeding aborts the kernel + returns
    /// `status: output_too_large`. SPA key: `max_output_bytes`.
    ///
    /// Plumbed into the GatewayClient when the operator overrides
    /// the default. The plumbing through `client.rs` is a v1.1
    /// follow-up — for now this field is stored but inert; the
    /// const ceiling remains 50 MiB. The SPA still surfaces the
    /// setting so the operator can write it, and the value is
    /// already round-tripped through the vault — the next service
    /// commit reads it.
    pub max_output_bytes: Option<usize>,
}

impl PythonSandboxService {
    /// Construct against a running gateway. `gateway_url` is the
    /// supervisor-published URL (e.g. `http://127.0.0.1:8501`);
    /// `work_root` is the host-side dir bind-mounted into the
    /// container at `/work` (matches the `state://work` mount in
    /// the plugin manifest); `artifacts_root` is where streaming
    /// publish writes content-addressed blobs; `events` is the
    /// process-wide EventBus the watcher's publish callback uses
    /// to emit attachment cards so the SPA chip appears immediately.
    ///
    /// 2026-05-18 — kept as a passthrough for callers (tests
    /// mostly) that don't care about settings; production boot
    /// uses `new_with_settings` so the SPA panel's knobs take
    /// effect.
    pub fn new(
        gateway_url: impl Into<String>,
        work_root: PathBuf,
        artifacts_root: PathBuf,
        db: Database,
        events: EventBus,
    ) -> Result<Arc<Self>, ServiceError> {
        Self::new_with_settings(
            gateway_url,
            work_root,
            artifacts_root,
            db,
            events,
            PythonSandboxSettings::default(),
        )
    }

    /// Same as [`new`] but threads operator-tunable settings
    /// (idle timeout, output cap) read from the plugin-settings
    /// vault into the KernelPool + GatewayClient. Settings with
    /// `None` fall back to the module-level defaults.
    pub fn new_with_settings(
        gateway_url: impl Into<String>,
        work_root: PathBuf,
        artifacts_root: PathBuf,
        db: Database,
        events: EventBus,
        settings: PythonSandboxSettings,
    ) -> Result<Arc<Self>, ServiceError> {
        let client = GatewayClient::new(gateway_url.into())?;
        let pool = match settings.idle_timeout {
            Some(d) => {
                tracing::info!(
                    idle_timeout_secs = d.as_secs(),
                    "python_sandbox: kernel pool using operator-configured idle timeout"
                );
                KernelPool::with_idle_timeout(client, d)
            }
            None => KernelPool::new(client),
        };
        // max_output_bytes is captured into the struct for future
        // GatewayClient plumbing but unused today — log when the
        // operator set a non-default so they get a breadcrumb
        // explaining why their setting "doesn't apply yet".
        if let Some(cap) = settings.max_output_bytes {
            tracing::info!(
                max_output_bytes = cap,
                "python_sandbox: max_output_bytes setting recorded; \
                 plumbing into GatewayClient is a v1.1 follow-up — \
                 the hard cap remains 50 MiB this release"
            );
        }

        // Watcher callback: publish each finished output as a
        // plugin artifact + emit the attachment card. Spawns the
        // actual work onto tokio so the watcher's timer task isn't
        // blocked by HTTP + disk I/O + card broadcast.
        let db_for_watcher = db.clone();
        let artifacts_for_watcher = artifacts_root.clone();
        let events_for_watcher = events.clone();
        let watcher = OutputWatcher::start(work_root.clone(), DEFAULT_DEBOUNCE, move |event| {
            let db = db_for_watcher.clone();
            let artifacts_root = artifacts_for_watcher.clone();
            let events = events_for_watcher.clone();
            tokio::spawn(async move {
                publish_output(db, artifacts_root, events, event).await;
            });
        })?;

        let _eviction_handle = pool.spawn_eviction_loop();

        Ok(Arc::new(Self {
            pool,
            work_root,
            artifacts_root,
            db,
            hydrated_convos: Arc::new(Mutex::new(HashMap::new())),
            _eviction_handle,
            _watcher: watcher,
        }))
    }

    pub fn pool(&self) -> &KernelPool {
        &self.pool
    }
    pub fn work_root(&self) -> &Path {
        &self.work_root
    }
    pub fn artifacts_root(&self) -> &Path {
        &self.artifacts_root
    }

    /// Hydrate this conversation's uploaded attachments into
    /// /work/<convo>/uploads/ when the attachment set has changed
    /// since the last hydration. Called from python.execute before
    /// the first ensure_for so the kernel can immediately read
    /// user-supplied files.
    ///
    /// Watermark strategy: on each call we run a cheap
    /// `SELECT MAX(received_at)` against `state_attachments` for
    /// the conversation, compare against the cached value, and
    /// short-circuit when they match. A mismatch (or absent
    /// cache entry) triggers the full `list_for_conversation` +
    /// `hydrate_uploads` walk. This costs one indexed query per
    /// execute when the cache is warm (~100 µs) but the upside
    /// is that the cache self-heals on ANY new attachment
    /// regardless of which code path wrote it (web composer,
    /// transport-inbound, `web_fetch(as_attachment: true)`).
    ///
    /// Pre-2026-05-25 the cache was a boolean `HashSet<convo>`;
    /// once a conversation hydrated once, subsequent executes
    /// skipped hydration entirely. That hid newly-arrived
    /// attachments and broke the `web_fetch → python.execute`
    /// bridge that lets the agent ingest remote datasets.
    pub async fn hydrate_if_needed(
        &self,
        convo: &ConversationId,
    ) -> Result<(), HydrateOnExecuteError> {
        // 1. Watermark probe — one cheap indexed read. `COALESCE`
        //    so an empty attachment set returns 0 instead of NULL.
        let convo_for_max = convo.clone();
        let db_for_max = self.db.clone();
        let current_max: i64 = tokio::task::spawn_blocking(move || {
            let cid = convo_for_max.as_str().to_owned();
            db_for_max.with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COALESCE(MAX(received_at), 0) \
                     FROM state_attachments WHERE conversation_id = ?1",
                    rusqlite::params![cid],
                    |r| r.get::<_, i64>(0),
                )?)
            })
        })
        .await
        .map_err(|e| HydrateOnExecuteError::Db {
            convo: convo.as_str().to_string(),
            source: execlaw_core::DbError::Migration(format!("watermark join: {e}")),
        })?
        .map_err(|e| HydrateOnExecuteError::Db {
            convo: convo.as_str().to_string(),
            source: e,
        })?;

        // 2. Compare against cached watermark. Match → skip.
        {
            let cache = self.hydrated_convos.lock().await;
            if cache.get(convo).copied() == Some(current_max) {
                return Ok(());
            }
        }

        // 3. Mismatch (or first-time): walk the full attachment set.
        let convo_owned = convo.clone();
        let db = self.db.clone();
        let rows = tokio::task::spawn_blocking(move || {
            AttachmentStore::new(&db).list_for_conversation(&convo_owned)
        })
        .await
        .map_err(|e| HydrateOnExecuteError::Db {
            convo: convo.as_str().to_string(),
            source: execlaw_core::DbError::Migration(format!("list join: {e}")),
        })?
        .map_err(|e| HydrateOnExecuteError::Db {
            convo: convo.as_str().to_string(),
            source: e,
        })?;

        if rows.is_empty() {
            // Still cache the watermark (0) so we don't redo the
            // SELECT MAX on every execute for an attachment-less
            // conversation.
            self.hydrated_convos
                .lock()
                .await
                .insert(convo.clone(), current_max);
            return Ok(());
        }

        let attachments: Vec<AttachmentToHydrate> = rows
            .into_iter()
            .map(|r| AttachmentToHydrate {
                blob_path: PathBuf::from(&r.path),
                filename: r
                    .filename
                    .unwrap_or_else(|| derive_default_filename(&r.mime_type, &r.sha256)),
            })
            .collect();

        let work_root = self.work_root.clone();
        let convo_for_blocking = convo.clone();
        let result = tokio::task::spawn_blocking(move || {
            hydrate_uploads(
                &work_root,
                &convo_for_blocking,
                &attachments,
                HydrateOpts::default(),
            )
        })
        .await
        .map_err(|e| HydrateOnExecuteError::Hydrate {
            convo: convo.as_str().to_string(),
            source: HydrationError::Io {
                path: self.work_root.clone(),
                source: std::io::Error::new(std::io::ErrorKind::Other, format!("join: {e}")),
            },
        })?
        .map_err(|e| HydrateOnExecuteError::Hydrate {
            convo: convo.as_str().to_string(),
            source: e,
        })?;

        tracing::info!(
            convo = %convo,
            files = result.len(),
            watermark = current_max,
            "python_sandbox hydrated conversation uploads"
        );
        self.hydrated_convos
            .lock()
            .await
            .insert(convo.clone(), current_max);
        Ok(())
    }

    /// Hook for the chats.rs conversation-delete path: drop the
    /// kernel server-side, remove the conversation's `/work/<convo>/`
    /// subdir, clear our hydration cache entry. Idempotent —
    /// no-op if nothing exists for this convo.
    pub async fn on_conversation_deleted(&self, convo: &ConversationId) {
        if let Err(e) = self.pool.drop_kernel(convo).await {
            tracing::warn!(
                ?e,
                %convo,
                "python_sandbox: drop_kernel during conversation delete failed; gateway will idle-cull"
            );
        }
        let convo_root = self.work_root.join(convo.as_str());
        if convo_root.exists() {
            if let Err(e) = std::fs::remove_dir_all(&convo_root) {
                tracing::warn!(
                    ?e,
                    path = %convo_root.display(),
                    "python_sandbox: removing /work/<convo>/ failed"
                );
            }
        }
        self.hydrated_convos.lock().await.remove(convo);
        tracing::info!(%convo, "python_sandbox cleaned up after conversation delete");
    }
}

/// Best-effort default filename when `state_attachments.filename`
/// is NULL — usually an attachment minted before migration 0006 or
/// by an ingest path that doesn't yet populate the column.
///
/// `attachment-<short_sha>.<ext_from_mime>` so the agent sees
/// something predictable rather than the raw sha256. The 8-char
/// prefix keeps it short while preserving uniqueness within one
/// conversation (collision probability for ~10K attachments is
/// negligible).
/// Derive a stable display + on-disk filename for an attachment row
/// whose `filename` column is null (legacy rows that pre-date
/// migration 0006, or transport-bridge ingest paths that don't
/// surface an original filename). The same value is used in TWO
/// places — they MUST match exactly:
///
///   * `crate::python_sandbox::service` hydration writes the blob
///     to `/work/<convo>/uploads/<this name>`.
///   * `crate::chats::attachments::build_attached_files_block` lists
///     the same name to the agent in the per-turn prose block so
///     it knows what to `open()` / `pandas.read_csv()`.
///
/// Format: `attachment-<8-hex-prefix-of-sha256>.<mime-derived-ext>`.
/// The 8-hex prefix collisions only after ~4 billion attachments
/// per conversation (sha256 short prefix), which is comfortably
/// beyond v1 scope; if the operator hits it they'll see an
/// overwrite, not a misroute. The mime→ext table accepts the v1
/// allowlist; unknown MIMEs fall back to `.bin` so the agent at
/// least gets a fileish reference.
pub(crate) fn derive_default_filename(mime: &str, sha: &str) -> String {
    let short = sha.get(..8).unwrap_or(sha);
    let ext = match mime {
        "text/csv" => "csv",
        "text/tab-separated-values" => "tsv",
        "application/json" => "json",
        "application/vnd.apache.parquet" => "parquet",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-excel" => "xls",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "text/markdown" => "md",
        "text/plain" => "txt",
        "application/pdf" => "pdf",
        _ => "bin",
    };
    format!("attachment-{short}.{ext}")
}

/// Run the path-based streaming publish for one watcher event.
/// Kept as a free function (not a method) so the watcher closure
/// can capture exactly what it needs without dragging the whole
/// service in via `Arc<Self>`.
///
/// Phase 8c: after the artifact lands in state_artifacts, emits an
/// attachment card open+close pair on the conversation's event
/// stream. The SPA's existing card projection turns that into a
/// download chip. Mirrors `ServerAttachmentApi::send`'s card-emit
/// shape so the SPA renderer can't tell our origin from any other
/// attachment.
async fn publish_output(
    db: Database,
    artifacts_root: PathBuf,
    events: EventBus,
    event: OutputCreated,
) {
    // file_name is None only when path ends in `..` — `is_in_outputs_dir`
    // rejects those before we get here, so the unwrap_or is defensive.
    let filename = event
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output.bin")
        .to_string();
    let mime = guess_mime(&event.path);
    let now = chrono::Utc::now().timestamp();

    // AttachmentStore::insert_plugin_artifact_from_path is blocking
    // (file I/O + sha256 + sqlite). Run it on a blocking thread so
    // we don't stall the tokio runtime under heavy publish bursts.
    let path = event.path.clone();
    let plugin_id = PLUGIN_ID.to_string();
    // Clone before moving into the closure — both filename and
    // mime are used in tracing + card emit after the publish call.
    let db_for_task = db.clone();
    let artifacts_for_task = artifacts_root.clone();
    let filename_for_task = filename.clone();
    let mime_for_task = mime.clone();
    let publish = tokio::task::spawn_blocking(move || {
        AttachmentStore::new(&db_for_task).insert_plugin_artifact_from_path(
            &artifacts_for_task,
            &plugin_id,
            &filename_for_task,
            &mime_for_task,
            &path,
            None, // no TTL — operator's deletion sweeper handles lifecycle
            now,
        )
    })
    .await;

    match publish {
        Ok(Ok(created)) => {
            tracing::info!(
                convo = %event.conversation_id,
                attachment_id = %created.attachment_id,
                filename = %filename,
                size = created.size_bytes,
                "python_sandbox published kernel output as artifact"
            );
            // Emit attachment-card open+close so the SPA chip renders.
            // Best-effort: a card-emit failure logs but doesn't fail
            // the publish (the artifact is committed; the operator
            // sees it on the next Files-pane refresh).
            if let Err(e) = emit_attachment_card(
                &db,
                &events,
                &event.conversation_id,
                &created.attachment_id,
                &filename,
                &mime,
                created.size_bytes,
            ) {
                tracing::warn!(
                    ?e,
                    convo = %event.conversation_id,
                    attachment_id = %created.attachment_id,
                    "python_sandbox card emit failed; artifact exists but chip won't render until refresh"
                );
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(
                ?e,
                path = %event.path.display(),
                "python_sandbox publish failed; output is still on disk in /work/<convo>/outputs/"
            );
        }
        Err(e) => {
            tracing::warn!(?e, "publish spawn_blocking join failed");
        }
    }
}

/// Emit the open+close card pair for one freshly-published
/// artifact. Mirrors `ServerAttachmentApi::send`'s card shape so
/// the SPA's existing renderer dispatches us identically to any
/// other attachment delivery — operator can't tell our chips from
/// research-PDF chips or signal-attachment chips.
fn emit_attachment_card(
    db: &Database,
    events: &EventBus,
    conversation_id: &ConversationId,
    attachment_id: &str,
    filename: &str,
    mime: &str,
    byte_size: u64,
) -> Result<(), String> {
    let download_url = format!("/api/attachments/{attachment_id}");
    let card_id = format!("att-{attachment_id}");
    let summary = format!("{filename} ({mime})");
    let details = serde_json::json!({
        "attachment_id": attachment_id,
        "filename": filename,
        "mime_type": mime,
        "byte_size": byte_size as i64,
        "download_url": download_url,
        // No operator-supplied caption from a watcher event — the
        // caption channel exists for manual `AttachmentApi.send`
        // calls where the agent passes context text.
        "caption": serde_json::Value::Null,
    });

    open_card_and_broadcast(
        db,
        events,
        conversation_id,
        "agent",
        &CardOpenedPayload {
            card_id: card_id.clone(),
            kind: CardKind::Attachment,
            title: filename.to_string(),
            summary: summary.clone(),
            state: Some(CardState::Running),
            details: details.clone(),
            actions: vec![CardAction::OpenDetail {
                href: download_url.clone(),
            }],
        },
    )
    .map_err(|e| format!("open card: {e}"))?;
    close_card_and_broadcast(
        db,
        events,
        conversation_id,
        "agent",
        &CardClosedPayload {
            card_id,
            state: CardState::Completed,
            summary,
            details: Some(details),
            attachment_id: Some(attachment_id.to_string()),
            error: None,
        },
    )
    .map_err(|e| format!("close card: {e}"))?;
    Ok(())
}

/// Best-effort MIME guess from extension. We don't pull a full
/// MIME database in — the formats we publish are dominated by
/// `text/csv`, `application/vnd.apache.parquet`, `image/png`,
/// `application/json`, with a long tail handled as
/// `application/octet-stream`.
fn guess_mime(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "parquet" => "application/vnd.apache.parquet",
        "arrow" | "ipc" => "application/vnd.apache.arrow.file",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "log" => "text/plain",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_default_filename_picks_extension_from_mime() {
        assert_eq!(
            derive_default_filename("text/csv", "abcdef1234567890"),
            "attachment-abcdef12.csv"
        );
        assert_eq!(
            derive_default_filename("application/vnd.apache.parquet", "feedfacedeadbeef"),
            "attachment-feedface.parquet"
        );
        assert_eq!(
            derive_default_filename("image/png", "abcd"),
            "attachment-abcd.png"
        );
        assert_eq!(
            derive_default_filename("application/octet-stream", "1234567890ab"),
            "attachment-12345678.bin"
        );
        // Unknown mime → bin extension
        assert_eq!(
            derive_default_filename("totally/made-up", "12345678"),
            "attachment-12345678.bin"
        );
    }

    // ===== Hydration watermark query (Slice F, 2026-05-25) =========
    //
    // `hydrate_if_needed` short-circuits when the cached
    // `MAX(received_at)` for a conversation matches the current
    // database value. The SQL below is the load-bearing piece —
    // pin it directly so a regression in the COALESCE / index hint
    // doesn't silently start returning NULL and crashing the
    // sandbox path.
    #[tokio::test]
    async fn watermark_query_returns_zero_for_empty_conversation_and_moves_up_on_insert() {
        use execlaw_core::DbConfig;
        use execlaw_core::MigrationRunner;
        use execlaw_core::attachments::{AttachmentRow, AttachmentStore};
        use execlaw_core::conversation::{
            ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
        };
        use execlaw_core::ids::{AttachmentId, EventSeq};

        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let cid = ConversationId::from("c-watermark");
        ConversationStore::new(&db)
            .upsert(&ConversationRow {
                conversation_id: cid.clone(),
                kind: ConversationKind::ControllerDM,
                last_seq: EventSeq(0),
                phase: Phase::Idle,
                controller_id: None,
                trust_class: "Controller".into(),
                snapshot_blob: None,
                snapshot_seq: None,
                lease_owner: None,
                lease_expires: None,
                modality: Modality::Text,
                display_name: None,
                display_name_source: "auto".into(),
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();

        // The same query `hydrate_if_needed` runs.
        let probe = |cid: &ConversationId| -> i64 {
            let cid_owned = cid.as_str().to_owned();
            db.with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COALESCE(MAX(received_at), 0) \
                     FROM state_attachments WHERE conversation_id = ?1",
                    rusqlite::params![cid_owned],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap()
        };

        // Empty → 0 (not NULL — COALESCE is doing its job).
        assert_eq!(probe(&cid), 0);

        // First attachment lands at received_at=100.
        AttachmentStore::new(&db)
            .insert(&AttachmentRow {
                id: AttachmentId::new(),
                conversation_id: cid.clone(),
                mime_type: "text/csv".into(),
                path: "/tmp/a.csv".into(),
                sha256: "aa".into(),
                received_at: 100,
                filename: Some("a.csv".into()),
            })
            .unwrap();
        assert_eq!(probe(&cid), 100);

        // Second attachment at received_at=200 — watermark moves up.
        // This is the case web_fetch(as_attachment: true) exercises:
        // a previously-hydrated conversation gets a new file mid-turn.
        AttachmentStore::new(&db)
            .insert(&AttachmentRow {
                id: AttachmentId::new(),
                conversation_id: cid.clone(),
                mime_type: "text/csv".into(),
                path: "/tmp/b.csv".into(),
                sha256: "bb".into(),
                received_at: 200,
                filename: Some("b.csv".into()),
            })
            .unwrap();
        assert_eq!(probe(&cid), 200);

        // Older attachment (received_at < current max) does NOT move
        // the watermark down — MAX semantics. Belt-and-suspenders
        // assert so a future cache-fix that switches to LAST(...) or
        // count() trips the test.
        AttachmentStore::new(&db)
            .insert(&AttachmentRow {
                id: AttachmentId::new(),
                conversation_id: cid.clone(),
                mime_type: "text/csv".into(),
                path: "/tmp/c.csv".into(),
                sha256: "cc".into(),
                received_at: 50,
                filename: Some("c.csv".into()),
            })
            .unwrap();
        assert_eq!(probe(&cid), 200);

        // Different conversation has its own watermark — no
        // cross-conversation bleed.
        let other = ConversationId::from("c-other");
        ConversationStore::new(&db)
            .upsert(&ConversationRow {
                conversation_id: other.clone(),
                kind: ConversationKind::ControllerDM,
                last_seq: EventSeq(0),
                phase: Phase::Idle,
                controller_id: None,
                trust_class: "Controller".into(),
                snapshot_blob: None,
                snapshot_seq: None,
                lease_owner: None,
                lease_expires: None,
                modality: Modality::Text,
                display_name: None,
                display_name_source: "auto".into(),
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        assert_eq!(probe(&other), 0);
    }

    #[test]
    fn guess_mime_covers_common_analyst_outputs() {
        assert_eq!(guess_mime(Path::new("/x/data.csv")), "text/csv");
        assert_eq!(
            guess_mime(Path::new("/x/regions.parquet")),
            "application/vnd.apache.parquet"
        );
        assert_eq!(guess_mime(Path::new("/x/chart.png")), "image/png");
        assert_eq!(guess_mime(Path::new("/x/result.json")), "application/json");
        assert_eq!(guess_mime(Path::new("/x/notes.md")), "text/markdown");
        // Case insensitivity
        assert_eq!(guess_mime(Path::new("/x/PHOTO.JPG")), "image/jpeg");
        // Unknown
        assert_eq!(
            guess_mime(Path::new("/x/blob.weird")),
            "application/octet-stream"
        );
        // No extension
        assert_eq!(
            guess_mime(Path::new("/x/no-ext")),
            "application/octet-stream"
        );
    }
}
