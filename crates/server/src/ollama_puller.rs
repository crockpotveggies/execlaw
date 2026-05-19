//! Active Ollama model pull for the Apple-Silicon native preset.
//!
//! The vLLM container path runs an `HfDownloader::ensure_model`
//! BEFORE spawning the container so the operator sees real progress
//! in the SPA pill. Ollama can't follow the same pattern — `ollama
//! serve` spawns instantly, binds its port, and reports healthy on
//! `/api/tags` before any model is in the cache. The first chat
//! completion then 404s with `model 'X' not found`.
//!
//! This module is the post-daemon-up equivalent: once Ollama answers
//! `/api/tags`, the backend supervisor checks whether the configured
//! model is in the cache (via [`is_model_present`]) and, if not,
//! POSTs to `/api/pull` and streams the response into a
//! `DownloadProgress` snapshot. The backend stays in
//! `LifecycleStage::DownloadingModel` until the pull finishes.
//!
//! Ollama's `/api/pull` streams JSON Lines like:
//!
//! ```jsonc
//! {"status":"pulling manifest"}
//! {"status":"downloading","digest":"sha256:abc","total":4096000,"completed":1024000}
//! {"status":"downloading","digest":"sha256:abc","total":4096000,"completed":2048000}
//! {"status":"verifying sha256 digest"}
//! {"status":"writing manifest"}
//! {"status":"removing any unused layers"}
//! {"status":"success"}
//! ```
//!
//! Each progress chunk's `total`+`completed` reflect the CURRENT
//! file being pulled (multi-layer models emit several rounds). We
//! surface those bytes straight through — the SPA pill already
//! renders `X / Y GB · NN%` and the per-file progression reads as a
//! steadily-advancing aggregate to the operator.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::time::Duration;

/// Loopback probe budget — `/api/tags` is a sub-millisecond response
/// once the daemon is up. We keep the timeout tight so a hung
/// daemon doesn't stall the supervisor reconcile.
const TAGS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on a single `/api/pull` call. The 32 B Qwen quant is
/// ~20 GB; on a 50 Mbps connection that's ~55 min. 90 minutes leaves
/// headroom for slower lines without letting a wedged stream hold
/// the slot in `DownloadingModel` forever.
const PULL_TIMEOUT: Duration = Duration::from_secs(90 * 60);

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Debug, Deserialize)]
struct TagEntry {
    name: String,
}

/// Check whether `model_id` is in the local Ollama model store.
///
/// Calls `GET http://127.0.0.1:{host_port}/api/tags` and looks for
/// an entry whose `name` matches `model_id` exactly. Ollama is
/// tag-strict — `qwen2.5:7b` and `qwen2.5:7b-instruct-q4_K_M` are
/// distinct names, so we don't fuzzy-match.
///
/// Returns `Ok(true)` when the model is cached, `Ok(false)` when
/// the daemon answered but the model isn't in the list, and `Err`
/// when the call itself failed (connection refused, timeout, or
/// HTTP non-2xx). The supervisor treats `Err` the same as
/// `Ok(false)` for the "kick a pull" decision but logs it.
pub async fn is_model_present(host_port: u16, model_id: &str) -> Result<bool> {
    let url = format!("http://127.0.0.1:{host_port}/api/tags");
    let client = reqwest::Client::builder()
        .timeout(TAGS_PROBE_TIMEOUT)
        .build()
        .context("build reqwest client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("/api/tags returned HTTP {}", resp.status()));
    }
    let tags: TagsResponse = resp.json().await.context("parse /api/tags JSON body")?;
    Ok(tags.models.iter().any(|m| m.name == model_id))
}

/// One line of Ollama's streaming `/api/pull` response. Only the
/// fields the supervisor consumes are deserialized — the digest +
/// status strings flow through unread because the SPA's pill copy
/// reads from `DownloadProgress`, not the raw status text.
#[derive(Debug, Deserialize)]
struct PullChunk {
    status: String,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    completed: Option<u64>,
    /// Set by Ollama when the pull failed (auth, network, no such
    /// model in the registry). When present we surface it as a
    /// pull-task failure verbatim.
    #[serde(default)]
    error: Option<String>,
}

/// Stream `POST /api/pull?name=<model_id>` and invoke `on_progress`
/// for each `downloading` chunk with `(completed, total)` bytes.
///
/// Resolves `Ok(())` when Ollama emits a `{"status":"success"}`
/// terminator. Returns `Err`:
///   * on transport failure (connection refused mid-stream, etc.)
///   * when Ollama emits an `error` field
///   * when the stream ends without `success` (truncated response)
///
/// `on_progress` runs synchronously on the pull task's tokio thread
/// — it must not block (i.e. should be a `try_lock`-style mirror
/// into a shared progress struct, not a blocking `lock`). Skipping
/// an update on lock contention is acceptable: the supervisor will
/// pick up the latest snapshot on its next reconcile tick.
pub async fn pull_model<F>(host_port: u16, model_id: &str, mut on_progress: F) -> Result<()>
where
    F: FnMut(u64, u64),
{
    use futures::StreamExt;
    let url = format!("http://127.0.0.1:{host_port}/api/pull");
    let body = serde_json::json!({ "name": model_id, "stream": true });
    let client = reqwest::Client::builder()
        .timeout(PULL_TIMEOUT)
        .build()
        .context("build reqwest client")?;
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("/api/pull returned HTTP {}", resp.status()));
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("read /api/pull stream chunk")?;
        buf.extend_from_slice(&bytes);
        // Ollama emits one JSON object per newline-terminated line.
        // Drain complete lines; keep any partial trailing line in
        // the buffer for the next chunk.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            // splice [..pos] (excluding the \n) into a heap line.
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let trimmed = &line[..line.len().saturating_sub(1)];
            if trimmed.is_empty() {
                continue;
            }
            let parsed: PullChunk = match serde_json::from_slice(trimmed) {
                Ok(p) => p,
                Err(e) => {
                    // Don't fail the whole pull on a single
                    // malformed chunk — Ollama occasionally emits
                    // diagnostic lines outside the JSON contract.
                    // Log + skip.
                    tracing::warn!(
                        chunk_preview = %String::from_utf8_lossy(
                            &trimmed[..trimmed.len().min(120)]
                        ),
                        "ollama pull: ignoring malformed chunk: {e}"
                    );
                    continue;
                }
            };
            if let Some(err) = parsed.error {
                return Err(anyhow!("ollama pull error: {err}"));
            }
            if let (Some(total), Some(completed)) = (parsed.total, parsed.completed) {
                on_progress(completed, total);
            }
            if parsed.status == "success" {
                return Ok(());
            }
        }
    }
    Err(anyhow!(
        "ollama pull stream for {model_id} ended without a 'success' terminator"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_chunk_deserializes_progress_line() {
        let line =
            br#"{"status":"downloading","digest":"sha256:abc","total":1024,"completed":512}"#;
        let p: PullChunk = serde_json::from_slice(line).unwrap();
        assert_eq!(p.status, "downloading");
        assert_eq!(p.total, Some(1024));
        assert_eq!(p.completed, Some(512));
        assert!(p.error.is_none());
    }

    #[test]
    fn pull_chunk_deserializes_status_only_line() {
        // Phase transitions (pulling manifest, verifying, etc.) have
        // no byte totals — both Option fields must be None.
        let line = br#"{"status":"verifying sha256 digest"}"#;
        let p: PullChunk = serde_json::from_slice(line).unwrap();
        assert_eq!(p.status, "verifying sha256 digest");
        assert!(p.total.is_none());
        assert!(p.completed.is_none());
    }

    #[test]
    fn pull_chunk_deserializes_error_line() {
        let line = br#"{"status":"error","error":"model 'nonexistent' not found"}"#;
        let p: PullChunk = serde_json::from_slice(line).unwrap();
        assert_eq!(p.error.as_deref(), Some("model 'nonexistent' not found"));
    }

    #[test]
    fn tags_response_deserializes_empty_cache() {
        // Fresh-install daemon answers `{"models":[]}` (or sometimes
        // omits the field entirely). Both must parse without error
        // and report the model as absent.
        let resp: TagsResponse = serde_json::from_slice(br#"{"models":[]}"#).unwrap();
        assert!(resp.models.is_empty());
        let resp: TagsResponse = serde_json::from_slice(br#"{}"#).unwrap();
        assert!(resp.models.is_empty());
    }

    #[test]
    fn tags_response_extracts_model_name() {
        let resp: TagsResponse = serde_json::from_slice(
            br#"{"models":[{"name":"qwen2.5:7b-instruct-q4_K_M","size":0}]}"#,
        )
        .unwrap();
        assert_eq!(resp.models.len(), 1);
        assert_eq!(resp.models[0].name, "qwen2.5:7b-instruct-q4_K_M");
    }
}
