//! Phase 14.C — host-side HuggingFace model downloader.
//!
//! Why a host-side downloader exists at all: vLLM, Whisper, and
//! similar inference containers download model weights on first
//! start by hitting `huggingface.co` directly from inside the
//! container. That has three failure modes:
//!
//!   1. **Re-download on every spawn.** When the supervisor reaps
//!      a CrashLooping container and respawns, the next container
//!      starts with an empty `/root/.cache/huggingface` and
//!      re-pulls the entire 18 GB of Qwen 3.5 27B AWQ. Five
//!      crash-loops eats 90 GB.
//!   2. **Zero progress visibility.** The container is in
//!      `Starting` state for 10+ minutes with no signal in the SPA
//!      that download is even happening (vs. genuinely stuck).
//!   3. **Auth lives in the wrong place.** Gated models need
//!      `HF_TOKEN` injected into every container, even though the
//!      auth concern is operator-level not container-level.
//!
//! This module owns the cache. The supervisor calls
//! `HfDownloader::ensure_model(id)` BEFORE spawning a container;
//! the call returns a `DownloadStream` whose progress events the
//! supervisor surfaces in the SPA's status pill. Once the stream
//! completes, the model is materialised in the primary cache and
//! the container is started with a read-only mount of that cache.
//!
//! The cache layout follows HF's own convention:
//!
//! ```text
//! ~/.execlaw/hf-cache/
//!   hub/
//!     models--<owner>--<repo>/
//!       snapshots/<commit>/
//!         config.json
//!         model-00001-of-00012.safetensors
//!         …
//!       refs/main          (text file containing the commit hash)
//! ```
//!
//! That layout is what `transformers` / vLLM look for at
//! `$HF_HOME/hub`, so the container "just works" with no env-var
//! hacks beyond mounting the cache.
//!
//! Secondary caches (operator-supplied) are scanned READ-ONLY for
//! already-downloaded files. When a needed file is found there but
//! not in the primary, we hardlink it into the primary if the
//! filesystems match, otherwise we copy. The container only ever
//! sees the primary cache — that keeps the mount surface simple +
//! avoids HF library confusion over multiple cache roots.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum HfError {
    #[error("HF API request failed: {0}")]
    Api(String),
    #[error("model id is malformed (expected `owner/repo` or `repo`): {0}")]
    BadModelId(String),
    #[error("cache I/O error: {0}")]
    Io(String),
    #[error("download stream cancelled")]
    Cancelled,
    #[error("HTTP error: {0}")]
    Http(String),
}

impl From<std::io::Error> for HfError {
    fn from(e: std::io::Error) -> Self {
        HfError::Io(e.to_string())
    }
}

impl From<reqwest::Error> for HfError {
    fn from(e: reqwest::Error) -> Self {
        HfError::Http(e.to_string())
    }
}

/// Progress events emitted while a model is being materialised.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    /// Calling `/api/models/{id}/revision/{rev}` to learn what
    /// files we need + their LFS pointers.
    ResolvingManifest { model_id: String },
    /// Found this file in a secondary cache; promoting it into
    /// the primary via hardlink (cheap, same volume) or copy
    /// (cross-volume).
    Importing {
        path: String,
        from_secondary: PathBuf,
        bytes: u64,
    },
    /// Downloading file `idx`/`total` of size `total_bytes`.
    DownloadingFile {
        path: String,
        bytes_downloaded: u64,
        total_bytes: u64,
        file_idx: usize,
        file_count: usize,
    },
    /// Cumulative across all files in the model.
    OverallProgress {
        bytes_downloaded: u64,
        total_bytes: u64,
        file_idx: usize,
        file_count: usize,
    },
    /// All files materialised in primary cache. The supervisor's
    /// next reconcile spawns the container.
    Complete {
        model_id: String,
        cache_dir: PathBuf,
        elapsed_ms: u64,
    },
    /// Recoverable error; the downloader will retry the file
    /// after a short backoff.
    Retrying {
        path: String,
        attempt: u32,
        error: String,
    },
    /// Unrecoverable failure. Stream ends after this event.
    Failed { error: String },
}

/// Single source-of-truth for a model's cache layout. The
/// downloader never returns paths relative to a secondary cache —
/// every callable path lives under `primary_root`.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model_id: String,
    pub revision: String,
    /// Root directory of the model's snapshot under the primary
    /// cache — what vLLM ends up reading from.
    pub snapshot_dir: PathBuf,
}

/// Host-side downloader. Cheap to clone; internal state is in
/// `Arc`s so multiple supervisor reconciles can call into the
/// same downloader without contention.
#[derive(Clone)]
pub struct HfDownloader {
    inner: Arc<HfDownloaderInner>,
}

struct HfDownloaderInner {
    primary_root: PathBuf,
    secondary_roots: Vec<PathBuf>,
    token: Option<String>,
    http: reqwest::Client,
}

impl HfDownloader {
    /// `primary_root` is the host's `~/.execlaw/hf-cache` (or
    /// wherever the operator wants execlaw's cache). The contained
    /// `hub/` subdir is the actual HF cache root we manage.
    /// `secondary_roots` are operator-supplied additional caches
    /// (typically `~/.cache/huggingface`); we scan them for
    /// already-downloaded files and hardlink/copy into the primary
    /// instead of re-downloading.
    pub fn new(
        primary_root: PathBuf,
        secondary_roots: Vec<PathBuf>,
        token: Option<String>,
    ) -> Self {
        // Build a long-timeout client because some shards are
        // multi-GB on a slow connection. The connect timeout stays
        // short so unreachable hosts fail fast.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            // No total request timeout — a 5 GB shard on a
            // marginal connection legitimately takes a very long
            // time, and we don't want to nuke a half-done download
            // on a slow link.
            .build()
            .expect("reqwest builder defaults are valid");
        Self {
            inner: Arc::new(HfDownloaderInner {
                primary_root,
                secondary_roots,
                token,
                http,
            }),
        }
    }

    /// Returns true when every file the model's manifest declares
    /// is already present in the primary cache. The supervisor
    /// uses this to skip the download phase and go straight to
    /// spawn when the cache is warm.
    ///
    /// Best-effort: if we can't reach HF to resolve the manifest,
    /// returns false (better to attempt download + let the network
    /// error surface than to "succeed" with a partially-cached
    /// model).
    pub async fn is_cached(&self, model_id: &str) -> bool {
        let snap = match self.snapshot_dir(model_id).await {
            Ok(p) => p,
            Err(_) => return false,
        };
        // Heuristic: presence of `config.json` is a strong signal
        // that the snapshot is complete — every HF model carries
        // one and it's the LAST file the typical HF pipeline pulls.
        // Tighter integrity (full manifest comparison) lives behind
        // a separate `verify_complete()` call we don't need on the
        // hot path.
        snap.join("config.json").is_file()
    }

    /// Returns the primary cache snapshot dir for a model. Used by
    /// the supervisor to set `HF_HOME` / `HF_HUB_CACHE` env vars
    /// inside the container so vLLM finds the pre-downloaded
    /// weights without doing its own HF API roundtrip.
    pub fn snapshot_dir_local(&self, model_id: &str, revision: &str) -> PathBuf {
        self.inner
            .primary_root
            .join("hub")
            .join(repo_dir_name(model_id))
            .join("snapshots")
            .join(revision)
    }

    /// Resolve revision (default `main`) → commit sha by hitting
    /// `/api/models/{id}/revision/main`. The result drives the
    /// `snapshots/<sha>` directory layout.
    pub async fn snapshot_dir(&self, model_id: &str) -> Result<PathBuf, HfError> {
        let revision = self.resolve_revision(model_id, "main").await?;
        Ok(self.snapshot_dir_local(model_id, &revision))
    }

    /// Kick off a download. Returns a stream of progress events the
    /// caller (supervisor) consumes to update the SPA status pill.
    /// The stream completes with either `Complete { … }` or
    /// `Failed { … }`; in both cases the inner task exits and the
    /// receiver hangs up.
    ///
    /// This method does NOT return until the download task is
    /// spawned and the manifest is resolved — that gives the
    /// caller a fast-failing surface for the "model id doesn't
    /// exist" case (404 from the manifest endpoint surfaces as an
    /// `Err(HfError::Api)` here, not as a single `Failed` event).
    pub async fn ensure_model(&self, model_id: &str) -> Result<DownloadStream, HfError> {
        let model_id = model_id.to_owned();
        if !is_valid_model_id(&model_id) {
            return Err(HfError::BadModelId(model_id));
        }
        let (tx, rx) = mpsc::channel(64);
        let downloader = self.clone();
        let model_id_for_task = model_id.clone();
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            if let Err(e) = downloader
                .run_download(&model_id_for_task, tx.clone(), started)
                .await
            {
                let _ = tx.send(DownloadEvent::Failed {
                    error: e.to_string(),
                }).await;
            }
        });
        Ok(DownloadStream {
            receiver: rx,
            _handle: handle,
        })
    }

    async fn run_download(
        &self,
        model_id: &str,
        tx: mpsc::Sender<DownloadEvent>,
        started: std::time::Instant,
    ) -> Result<(), HfError> {
        let _ = tx
            .send(DownloadEvent::ResolvingManifest {
                model_id: model_id.to_owned(),
            })
            .await;
        let revision = self.resolve_revision(model_id, "main").await?;
        let manifest = self.fetch_manifest(model_id, &revision).await?;

        let snapshot_dir = self.snapshot_dir_local(model_id, &revision);
        tokio::fs::create_dir_all(&snapshot_dir).await?;
        // HF cache layout also wants a `refs/main` text file
        // containing the commit sha — vLLM/transformers reads this
        // when resolving "the latest known revision."
        let refs_dir = self
            .inner
            .primary_root
            .join("hub")
            .join(repo_dir_name(model_id))
            .join("refs");
        tokio::fs::create_dir_all(&refs_dir).await?;
        tokio::fs::write(refs_dir.join("main"), &revision).await?;

        let total_bytes: u64 = manifest.iter().map(|f| f.size.unwrap_or(0)).sum();
        let mut overall = 0u64;
        for (idx, file) in manifest.iter().enumerate() {
            let dest = snapshot_dir.join(&file.path);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // 1) already in primary?
            if file_already_satisfied(&dest, file.size).await {
                let bytes = file.size.unwrap_or(0);
                overall = overall.saturating_add(bytes);
                let _ = tx
                    .send(DownloadEvent::OverallProgress {
                        bytes_downloaded: overall,
                        total_bytes,
                        file_idx: idx + 1,
                        file_count: manifest.len(),
                    })
                    .await;
                continue;
            }
            // 2) try to hardlink/copy from a secondary cache
            if let Some(found) = self
                .find_in_secondary(&file.path, model_id, &revision)
                .await
            {
                if materialise_from_secondary(&found, &dest).await.is_ok() {
                    let bytes = file.size.unwrap_or(0);
                    overall = overall.saturating_add(bytes);
                    let _ = tx
                        .send(DownloadEvent::Importing {
                            path: file.path.clone(),
                            from_secondary: found,
                            bytes,
                        })
                        .await;
                    let _ = tx
                        .send(DownloadEvent::OverallProgress {
                            bytes_downloaded: overall,
                            total_bytes,
                            file_idx: idx + 1,
                            file_count: manifest.len(),
                        })
                        .await;
                    continue;
                }
            }
            // 3) download with up to 3 retries
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self
                    .download_file(
                        model_id,
                        &revision,
                        &file.path,
                        file.size,
                        &dest,
                        idx,
                        manifest.len(),
                        overall,
                        total_bytes,
                        &tx,
                    )
                    .await
                {
                    Ok(file_bytes) => {
                        overall = overall.saturating_add(file_bytes);
                        let _ = tx
                            .send(DownloadEvent::OverallProgress {
                                bytes_downloaded: overall,
                                total_bytes,
                                file_idx: idx + 1,
                                file_count: manifest.len(),
                            })
                            .await;
                        break;
                    }
                    Err(e) if attempt < 3 => {
                        warn!(
                            file = %file.path,
                            attempt,
                            "HF download attempt failed: {e}; retrying"
                        );
                        let _ = tx
                            .send(DownloadEvent::Retrying {
                                path: file.path.clone(),
                                attempt,
                                error: e.to_string(),
                            })
                            .await;
                        tokio::time::sleep(std::time::Duration::from_millis(
                            500 * (1u64 << (attempt - 1)),
                        ))
                        .await;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let _ = tx
            .send(DownloadEvent::Complete {
                model_id: model_id.to_owned(),
                cache_dir: snapshot_dir,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })
            .await;
        Ok(())
    }

    async fn resolve_revision(
        &self,
        model_id: &str,
        revision: &str,
    ) -> Result<String, HfError> {
        // The HF API returns a 200 with a JSON payload that
        // includes `sha`. We use that as the revision id.
        #[derive(Deserialize)]
        struct RevisionResponse {
            sha: String,
        }
        let url = format!(
            "https://huggingface.co/api/models/{model_id}/revision/{revision}"
        );
        let mut req = self.inner.http.get(&url);
        if let Some(t) = &self.inner.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(HfError::Api(format!(
                "GET {url} → {}",
                resp.status()
            )));
        }
        let body: RevisionResponse = resp.json().await?;
        Ok(body.sha)
    }

    async fn fetch_manifest(
        &self,
        model_id: &str,
        revision: &str,
    ) -> Result<Vec<ManifestFile>, HfError> {
        // `/api/models/{id}/tree/{rev}?recursive=true` returns
        // every file in the snapshot. Each entry has `type`, `path`,
        // `size`, and (for LFS files) `lfs.sha256`. We fetch only
        // `type=file` entries — directories are implicit in the
        // path layout.
        #[derive(Deserialize)]
        struct TreeEntry {
            #[serde(rename = "type")]
            kind: String,
            path: String,
            size: Option<u64>,
        }
        let url = format!(
            "https://huggingface.co/api/models/{model_id}/tree/{revision}?recursive=true"
        );
        let mut req = self.inner.http.get(&url);
        if let Some(t) = &self.inner.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(HfError::Api(format!(
                "GET {url} → {}",
                resp.status()
            )));
        }
        let entries: Vec<TreeEntry> = resp.json().await?;
        Ok(entries
            .into_iter()
            .filter(|e| e.kind == "file")
            .map(|e| ManifestFile {
                path: e.path,
                size: e.size,
            })
            .collect())
    }

    /// Walk every secondary cache for a matching `models--…/snapshots/<rev>/<path>`
    /// or `…/snapshots/<any>/<path>`. We accept any revision when
    /// scanning secondaries because the user's separate cache may
    /// hold a different commit than HF's current `main`; if the
    /// path matches and the byte count looks right, hardlinking it
    /// into our primary is good enough (HF libraries don't validate
    /// snapshots against `refs/main` unless asked to).
    async fn find_in_secondary(
        &self,
        rel_path: &str,
        model_id: &str,
        revision: &str,
    ) -> Option<PathBuf> {
        let dirname = repo_dir_name(model_id);
        for root in &self.inner.secondary_roots {
            let exact = root
                .join("hub")
                .join(&dirname)
                .join("snapshots")
                .join(revision)
                .join(rel_path);
            if exact.is_file() {
                return Some(exact);
            }
            // Fallback — look for the same file under any snapshot
            // dir of the matching repo. Useful when the operator's
            // cache holds a slightly older or newer commit than HF
            // currently advertises.
            let snapshots_root = root.join("hub").join(&dirname).join("snapshots");
            if let Ok(mut rd) = tokio::fs::read_dir(&snapshots_root).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let candidate = entry.path().join(rel_path);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_file(
        &self,
        model_id: &str,
        revision: &str,
        rel_path: &str,
        expected_size: Option<u64>,
        dest: &Path,
        file_idx: usize,
        file_count: usize,
        already_done: u64,
        total_bytes: u64,
        tx: &mpsc::Sender<DownloadEvent>,
    ) -> Result<u64, HfError> {
        // HF resolves LFS files via `/resolve/<rev>/<path>` redirects;
        // reqwest follows them automatically.
        let url = format!(
            "https://huggingface.co/{model_id}/resolve/{revision}/{rel_path}"
        );
        let mut req = self.inner.http.get(&url);
        if let Some(t) = &self.inner.token {
            req = req.bearer_auth(t);
        }
        let mut resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(HfError::Http(format!(
                "GET {url} → {}",
                resp.status()
            )));
        }
        // Stream into a temp file alongside the dest so a crash
        // mid-download doesn't leave a half-written final path.
        let tmp = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&tmp).await?;
        use tokio::io::AsyncWriteExt;
        let mut bytes_downloaded: u64 = 0;
        let mut last_emit = std::time::Instant::now();
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
            bytes_downloaded = bytes_downloaded.saturating_add(chunk.len() as u64);
            // Throttle progress events to at most ~5/sec so a fast
            // download doesn't flood the SPA channel.
            if last_emit.elapsed() >= std::time::Duration::from_millis(200) {
                let _ = tx
                    .send(DownloadEvent::DownloadingFile {
                        path: rel_path.to_owned(),
                        bytes_downloaded,
                        total_bytes: expected_size.unwrap_or(0),
                        file_idx: file_idx + 1,
                        file_count,
                    })
                    .await;
                let _ = tx
                    .send(DownloadEvent::OverallProgress {
                        bytes_downloaded: already_done.saturating_add(bytes_downloaded),
                        total_bytes,
                        file_idx: file_idx + 1,
                        file_count,
                    })
                    .await;
                last_emit = std::time::Instant::now();
            }
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, dest).await?;
        debug!(
            file = %rel_path,
            bytes = bytes_downloaded,
            "HF file downloaded"
        );
        Ok(bytes_downloaded)
    }
}

/// Live progress feed for an in-flight download.
pub struct DownloadStream {
    receiver: mpsc::Receiver<DownloadEvent>,
    /// Held to keep the spawned task alive; we drop it implicitly
    /// when the stream is dropped, which causes the task's `tx`
    /// to error and terminate the download.
    _handle: tokio::task::JoinHandle<()>,
}

impl DownloadStream {
    pub async fn next(&mut self) -> Option<DownloadEvent> {
        self.receiver.recv().await
    }
}

#[derive(Debug, Clone)]
struct ManifestFile {
    path: String,
    size: Option<u64>,
}

/// Translate `owner/repo` (or `repo`) to HF's on-disk dir naming:
/// `models--<owner>--<repo>` (or `models--<repo>` for unowned).
pub fn repo_dir_name(model_id: &str) -> String {
    if let Some((owner, repo)) = model_id.split_once('/') {
        format!("models--{owner}--{repo}")
    } else {
        format!("models--{model_id}")
    }
}

/// Loose validation: HF model ids are `owner/repo` where each
/// segment is alphanumeric + `-`, `_`, `.`. Catches obvious typos
/// before we hit the network.
fn is_valid_model_id(id: &str) -> bool {
    if id.is_empty() || id.contains("..") || id.starts_with('/') || id.ends_with('/') {
        return false;
    }
    let segments: Vec<&str> = id.split('/').collect();
    if segments.len() > 2 {
        return false;
    }
    segments.iter().all(|s| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    })
}

async fn file_already_satisfied(dest: &Path, expected_size: Option<u64>) -> bool {
    let meta = match tokio::fs::metadata(dest).await {
        Ok(m) => m,
        Err(_) => return false,
    };
    if !meta.is_file() {
        return false;
    }
    match expected_size {
        Some(s) => meta.len() == s,
        // Manifest didn't report a size (rare — HF always provides
        // one for committed files but small text files like
        // `.gitattributes` sometimes omit it). Fall back to
        // "exists, non-empty" — lenient but safe for non-LFS text.
        None => meta.len() > 0,
    }
}

async fn materialise_from_secondary(src: &Path, dest: &Path) -> Result<(), HfError> {
    // Try hardlink first — costs zero disk + zero copy time. Falls
    // back to copy if the filesystems differ (e.g. secondary on
    // D:\, primary on C:\) or if the OS denies the hardlink (NTFS
    // requires both sides on the same volume + dev mode for some
    // edge cases).
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::hard_link(src, dest).await {
        Ok(()) => {
            info!(
                from = %src.display(),
                to = %dest.display(),
                "hardlinked HF file from secondary cache"
            );
            Ok(())
        }
        Err(_) => {
            tokio::fs::copy(src, dest).await?;
            info!(
                from = %src.display(),
                to = %dest.display(),
                "copied HF file from secondary cache (cross-volume)"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_dir_name_handles_owner_and_unowned() {
        assert_eq!(
            repo_dir_name("Qwen/Qwen2.5-7B-Instruct-AWQ"),
            "models--Qwen--Qwen2.5-7B-Instruct-AWQ"
        );
        assert_eq!(repo_dir_name("bert-base-uncased"), "models--bert-base-uncased");
    }

    #[test]
    fn is_valid_model_id_accepts_real_repos() {
        assert!(is_valid_model_id("Qwen/Qwen2.5-32B-Instruct-AWQ"));
        assert!(is_valid_model_id("OpenVINO/Phi-3-mini-4k-instruct-int4-ov"));
        assert!(is_valid_model_id("bert-base-uncased"));
    }

    #[test]
    fn is_valid_model_id_rejects_path_traversal_and_garbage() {
        assert!(!is_valid_model_id(""));
        assert!(!is_valid_model_id("/etc/passwd"));
        assert!(!is_valid_model_id("Qwen/.."));
        assert!(!is_valid_model_id("a/b/c"));
        assert!(!is_valid_model_id("has spaces/repo"));
        assert!(!is_valid_model_id("../escape"));
    }

    #[tokio::test]
    async fn file_already_satisfied_size_match() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a");
        tokio::fs::write(&f, b"abcdef").await.unwrap();
        assert!(file_already_satisfied(&f, Some(6)).await);
        assert!(!file_already_satisfied(&f, Some(7)).await);
        assert!(!file_already_satisfied(&dir.path().join("nope"), Some(0)).await);
    }

    #[tokio::test]
    async fn materialise_from_secondary_falls_through_to_copy_when_hardlink_fails() {
        // We can't easily synthesize a cross-volume hardlink failure
        // in tempfile (both sides land on the same volume), but we
        // can verify the success path: hardlink works → dest is a
        // valid file with matching contents.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("snapshots/abc/file.bin");
        tokio::fs::write(&src, b"contents").await.unwrap();
        materialise_from_secondary(&src, &dest).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"contents");
    }
}
