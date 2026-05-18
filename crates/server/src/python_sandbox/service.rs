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

use crate::python_sandbox::client::{GatewayClient, GatewayError};
use crate::python_sandbox::kernel_pool::KernelPool;
use crate::python_sandbox::output_watcher::{OutputCreated, OutputWatcher, DEFAULT_DEBOUNCE};
use execlaw_core::Database;
use execlaw_core::attachments::AttachmentStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

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
    _eviction_handle: tokio::task::JoinHandle<()>,
    // OutputWatcher dropped on Drop tears down its OS thread + tokio
    // timer. Kept named to make the lifetime obvious to readers.
    _watcher: OutputWatcher,
}

impl PythonSandboxService {
    /// Construct against a running gateway. `gateway_url` is the
    /// supervisor-published URL (e.g. `http://127.0.0.1:8501`);
    /// `work_root` is the host-side dir bind-mounted into the
    /// container at `/work` (matches the `state://work` mount in
    /// the plugin manifest); `artifacts_root` is where streaming
    /// publish writes content-addressed blobs.
    pub fn new(
        gateway_url: impl Into<String>,
        work_root: PathBuf,
        artifacts_root: PathBuf,
        db: Database,
    ) -> Result<Arc<Self>, ServiceError> {
        let client = GatewayClient::new(gateway_url.into())?;
        let pool = KernelPool::new(client);

        // Watcher callback: publish each finished output as a
        // plugin artifact. Spawns the actual work onto tokio so the
        // watcher's timer task isn't blocked by HTTP + disk I/O.
        let db_for_watcher = db.clone();
        let artifacts_for_watcher = artifacts_root.clone();
        let watcher = OutputWatcher::start(work_root.clone(), DEFAULT_DEBOUNCE, move |event| {
            let db = db_for_watcher.clone();
            let artifacts_root = artifacts_for_watcher.clone();
            tokio::spawn(async move {
                publish_output(db, artifacts_root, event).await;
            });
        })?;

        let _eviction_handle = pool.spawn_eviction_loop();

        Ok(Arc::new(Self {
            pool,
            work_root,
            artifacts_root,
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
}

/// Run the path-based streaming publish for one watcher event.
/// Kept as a free function (not a method) so the watcher closure
/// can capture exactly what it needs without dragging the whole
/// service in via `Arc<Self>`.
async fn publish_output(db: Database, artifacts_root: PathBuf, event: OutputCreated) {
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
    // Clone filename for the closure; the original stays available
    // for the post-publish tracing line.
    let filename_for_task = filename.clone();
    let publish = tokio::task::spawn_blocking(move || {
        AttachmentStore::new(&db).insert_plugin_artifact_from_path(
            &artifacts_root,
            &plugin_id,
            &filename_for_task,
            &mime,
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
            // Phase 8c follow-up: emit AttachmentApi::send card pair
            // here so the SPA renders the chip immediately. For now
            // the artifact exists in state_artifacts; the SPA picks
            // it up on next Files-pane refresh.
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
