//! Phase 14 — first-run setup preflight (`/api/admin/setup/preflight`).
//!
//! After the operator creates their controller account at
//! `POST /api/setup`, the SPA's wizard runs through two more screens
//! (Docker availability + GPU detection / backend setup). Both
//! screens depend on the same backend probe so a single endpoint
//! returns everything they need:
//!
//! ```json
//! {
//!   "docker": { "available": true, "version": "24.0.7" },
//!   "gpus":   [ { "vendor": "Nvidia", ... }, ... ]
//! }
//! ```
//!
//! Docker availability is determined by shelling out to `docker info`
//! — that connects to the daemon, so success means dockerd is up and
//! reachable, not just that the CLI is installed. The version comes
//! from `--format '{{.ServerVersion}}'`. Missing → `available: false,
//! version: null` and the SPA shows the Docker-Desktop install
//! prompt.
//!
//! GPUs come from the same `hardware-query`-backed `detect()` the
//! Backend wizard uses (Phase 14 follow-up).

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use execlaw_container_manager::{GpuDevice, NativeServiceController, detect};
use execlaw_core::audit::AuditStore;
use execlaw_core::general_settings::GeneralSettingsStore;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct PreflightResponse {
    pub docker: DockerStatus,
    /// Ollama availability for the Apple-Silicon native-runtime path.
    /// `available: true` means `NativeServiceController::discover_ollama`
    /// resolved a working binary; the wizard renders the model
    /// dropdown directly. `available: false` means we couldn't find
    /// one, and the wizard renders the install panel with
    /// `brew install ollama` instructions before the Save button.
    pub ollama: OllamaStatus,
    /// `Vec<GpuDevice>` — `GpuDevice` is in the container-manager
    /// crate which doesn't depend on utoipa, so we expose it as
    /// opaque JSON in the OpenAPI spec. SPA-side types live in
    /// `web/src/api/endpoints.ts` and stay in sync by hand.
    #[schema(value_type = serde_json::Value)]
    pub gpus: Vec<GpuDevice>,
    /// Free space, in bytes, on the volume that hosts execlaw's HF
    /// model cache (`~/.execlaw/hf-cache`). The setup wizard checks
    /// this against the picked model's expected on-disk size + a
    /// 5 GB safety margin and warns if there's not enough room.
    /// `None` when the platform's disk-free probe failed (rare;
    /// surfaced as a "couldn't detect free space" warning in the
    /// SPA — better safe than silent).
    pub disk_free_bytes: Option<u64>,
    /// Path the disk-free probe was run against. Surfaced so the
    /// operator knows where to free up space when they hit the
    /// "not enough room" warning.
    pub disk_free_path: Option<String>,
    /// HuggingFace model id → cached bytes on disk, for every model
    /// already present under `<cache>/hub/`. The setup wizard
    /// subtracts the picked model's cached bytes from "required
    /// disk space" so an operator who already has the weights
    /// downloaded doesn't see a false "not enough disk" error
    /// (the weights don't need to be re-acquired). Empty map on a
    /// fresh install or when the cache dir doesn't exist yet.
    pub cached_models: std::collections::HashMap<String, u64>,
}

/// Ollama detection result for the Apple-Silicon path. Surfaces both
/// to the first-run wizard (per-purpose Apple cards) and the ongoing
/// `/settings/backends` edit form, so an operator who installs Ollama
/// AFTER hitting "Recheck" sees the form switch from install-panel
/// to model-picker without a full page reload.
#[derive(Debug, Serialize, ToSchema)]
pub struct OllamaStatus {
    /// `true` when `NativeServiceController::discover_ollama` returned
    /// Ok AND the binary's `--version` invocation also succeeded. We
    /// gate on both because a stale symlink or permission denial
    /// would surface a discovered path that can't actually run, which
    /// would crash the supervisor at spawn time instead of producing
    /// a clean install-prompt UX up front.
    pub available: bool,
    /// Trimmed output of `ollama --version`, e.g. `"0.1.43"`. The
    /// wizard renders this as a small "Ollama X.Y.Z detected" badge so
    /// the operator confirms the right binary got picked when they
    /// have multiple installs (Homebrew + manual).
    pub version: Option<String>,
    /// Absolute path the discoverer landed on. Surfaced in the same
    /// confirmation badge so multi-install systems (Homebrew prefix
    /// vs. `~/bin`) read out unambiguously.
    pub path: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DockerStatus {
    /// `true` when `docker info` returned exit 0 — the CLI is on
    /// PATH AND the daemon answered. SPA gates the "managed
    /// backend" path on this.
    pub available: bool,
    /// Server version reported by `docker info --format
    /// '{{.ServerVersion}}'`. Surface in the SPA when present so
    /// the operator sees what the wizard talks to.
    pub version: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/setup/preflight",
    responses(
        (
            status = 200,
            description = "Docker availability + detected GPUs for the setup wizard",
            body = PreflightResponse,
        ),
    ),
    security(("bearer_jwt" = [])),
    tag = "setup"
)]
pub async fn get_handler(
    State(_state): State<AppState>,
    _user: AuthedUser,
) -> Json<PreflightResponse> {
    // `detect_docker` shells out to `docker info`, which can block
    // for tens of seconds when Docker Desktop is in a half-broken
    // state (running but not serving — observed under WSLg with a
    // Docker Desktop WSL-integration distro). The previous direct
    // call held the tokio worker for the entire blocked duration,
    // which made the SPA spin forever on the wizard's docker check.
    // Wrapping in `spawn_blocking` lifts the syscall off the runtime
    // so other requests still flow; the timeout inside
    // `detect_docker` caps the wait at ~3 seconds, after which we
    // surface `available: false` and let the operator decide.
    let docker = tokio::task::spawn_blocking(detect_docker)
        .await
        .unwrap_or(DockerStatus {
            available: false,
            version: None,
        });
    let ollama = detect_ollama();
    let profile = detect();
    let (disk_free_bytes, disk_free_path) = detect_disk_free();
    let cached_models = detect_cached_models();
    Json(PreflightResponse {
        docker,
        ollama,
        gpus: profile.gpus,
        disk_free_bytes,
        disk_free_path,
        cached_models,
    })
}

/// Resolve `~/.execlaw/hf-cache` (the host's primary HF model
/// cache) and ask the OS how much free space remains on the
/// volume that hosts it. The wizard compares this against the
/// picked model's expected size + a 5 GB safety margin.
///
/// Returns `(None, Some(path))` when the path resolution succeeded
/// but the disk-free syscall failed — preserves the path for
/// operator messaging. Returns `(None, None)` when even path
/// resolution failed (e.g. no `$HOME` on a misconfigured service
/// account) — the SPA renders "couldn't detect free space" in
/// that case.
fn detect_disk_free() -> (Option<u64>, Option<String>) {
    // Match the path the supervisor uses for its primary cache.
    // EXECLAW_HF_CACHE wins; otherwise `<data_dir>/hf-cache`.
    let path = match std::env::var("EXECLAW_HF_CACHE") {
        Ok(p) => Some(std::path::PathBuf::from(p)),
        Err(_) => {
            directories::ProjectDirs::from("", "", "execlaw").map(|d| d.data_dir().join("hf-cache"))
        }
    };
    let Some(path) = path else {
        return (None, None);
    };
    // Probe whichever ancestor exists — the cache dir itself may
    // not exist yet on a fresh install. Walk upward until we find
    // a real directory; that's the volume we'd actually write to.
    let mut probe = path.clone();
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p.to_path_buf(),
            None => return (None, Some(path.to_string_lossy().into_owned())),
        }
    }
    let path_str = path.to_string_lossy().into_owned();
    match disk_free_bytes_for(&probe) {
        Ok(b) => (Some(b), Some(path_str)),
        Err(e) => {
            tracing::warn!(
                path = %probe.display(),
                "disk-free probe failed: {e}; reporting unknown"
            );
            (None, Some(path_str))
        }
    }
}

/// Cross-platform free-space probe via the `fs4` crate, which
/// wraps `statvfs` (POSIX) and `GetDiskFreeSpaceExW` (Windows)
/// safely. We use this instead of hand-rolling FFI because the
/// server crate forbids unsafe code.
fn disk_free_bytes_for(path: &std::path::Path) -> std::io::Result<u64> {
    fs4::available_space(path)
}

/// Enumerate the HF cache and return `model_id → bytes_on_disk` for
/// every model present. The HF cache layout is
/// `<cache>/hub/models--<owner>--<repo>/...`, so we read directory
/// entries under `<cache>/hub/`, strip the `models--` prefix, and
/// convert the first `--` back to `/` to recover the canonical
/// `owner/repo` model id. Sizes are summed via a recursive walk.
///
/// Returns an empty map when:
///   * the HF cache dir doesn't exist yet (fresh install)
///   * `<cache>/hub` doesn't exist
///   * any I/O error mid-walk (best-effort; we don't fail preflight
///     over a stale symlink in someone's cache)
///
/// Drives the wizard's "this model is already cached" path: the
/// frontend looks up the picked model's id in this map, subtracts
/// those bytes from `required = disk_mb + safety_margin`, and
/// suppresses the "not enough disk" warning when the cached bytes
/// cover the weights. Catches the regression where an operator with
/// the model fully downloaded saw a false-positive "free up 3.1 GB"
/// because the volume was full but the model didn't need any
/// additional bytes.
fn detect_cached_models() -> std::collections::HashMap<String, u64> {
    let cache_root = match std::env::var("EXECLAW_HF_CACHE") {
        Ok(p) => Some(std::path::PathBuf::from(p)),
        Err(_) => {
            directories::ProjectDirs::from("", "", "execlaw").map(|d| d.data_dir().join("hf-cache"))
        }
    };
    let Some(root) = cache_root else {
        return std::collections::HashMap::new();
    };
    cached_models_in(&root)
}

/// Pure helper: enumerate `<cache_root>/hub/models--*` and produce
/// the `model_id → bytes` map. Split out from `detect_cached_models`
/// so tests can point at a temp directory without touching env vars
/// (the crate forbids unsafe code, and `std::env::set_var` is
/// unsafe in the 2024 edition).
pub(crate) fn cached_models_in(
    cache_root: &std::path::Path,
) -> std::collections::HashMap<String, u64> {
    let hub = cache_root.join("hub");
    let entries = match std::fs::read_dir(&hub) {
        Ok(e) => e,
        Err(_) => return std::collections::HashMap::new(),
    };
    let mut out = std::collections::HashMap::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let stripped = match name.strip_prefix("models--") {
            Some(s) => s,
            None => continue,
        };
        // HF stores `owner/repo` as `owner--repo` in the directory
        // name. Replace the first `--` with `/`. HF model ids are
        // always two-part (`org/name`), so a single replacement is
        // exact — but we use `splitn` rather than `replacen` to be
        // explicit about the structure.
        let model_id = match stripped.splitn(2, "--").collect::<Vec<_>>().as_slice() {
            [owner, repo] => format!("{owner}/{repo}"),
            _ => continue,
        };
        let bytes = dir_size(&entry.path()).unwrap_or(0);
        if bytes > 0 {
            out.insert(model_id, bytes);
        }
    }
    out
}

/// Recursive directory size in bytes. Skips entries whose metadata
/// or directory walk fails — the cache is operator-managed and we
/// don't want a single broken symlink to fail the whole probe.
fn dir_size(path: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let entries = match std::fs::read_dir(&p) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
            // Symlinks: meta.is_dir/is_file follow the link target, so
            // we land on the linked file's bytes if the target is
            // valid. A dangling symlink falls through both branches.
        }
    }
    Ok(total)
}

/// Probe `docker info` to determine Docker daemon availability.
///
/// We use `docker info --format` instead of `docker --version`
/// because the latter only checks the CLI binary; `info` actually
/// connects to the daemon. A daemon-down state (Docker Desktop
/// quit, dockerd not started) reads as `available: false` — which
/// is the right answer for the wizard's "can we manage backends?"
/// question.
///
/// PATH handling: the launchd-spawned server inherits the minimal
/// default PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), which does NOT
/// include `/usr/local/bin/` or `/opt/homebrew/bin/`. Docker
/// Desktop installs the CLI at `/usr/local/bin/docker` →
/// `/Applications/Docker.app/Contents/Resources/bin/docker`, so
/// `Command::new("docker")` returns `program not found` even
/// though the daemon is up and the sidecar supervisor (which uses
/// the Unix socket via Bollard) can talk to it just fine. We try
/// `docker` on PATH first for the dev-shell case, then fall back
/// to absolute paths at the well-known install locations. The
/// first one that exits 0 wins.
fn detect_docker() -> DockerStatus {
    // PATH probe first — covers `cargo run` from a dev shell and
    // operators who put docker somewhere non-standard but in PATH.
    if let Some(s) = try_docker_at("docker") {
        return s;
    }
    // Absolute-path fallbacks for the launchd-spawn case where
    // PATH is minimal. Order matters: macOS Docker Desktop's
    // symlink at /usr/local/bin/docker is the most common; the
    // brew-installed `docker` CLI on Apple-Silicon brew lives at
    // /opt/homebrew/bin/docker; the .app's internal bin directory
    // works as a last resort even without the /usr/local/ symlink.
    for path in [
        "/usr/local/bin/docker",
        "/opt/homebrew/bin/docker",
        "/Applications/Docker.app/Contents/Resources/bin/docker",
    ] {
        if let Some(s) = try_docker_at(path) {
            return s;
        }
    }
    DockerStatus {
        available: false,
        version: None,
    }
}

/// Run `<binary> info --format '{{.ServerVersion}}'` and translate
/// the result into `Option<DockerStatus>`. Returns `Some` when the
/// command exited 0 (Docker is reachable) and `None` otherwise so
/// the caller can fall through to the next candidate path.
///
/// Bounded by [`DOCKER_PROBE_TIMEOUT`]: a daemon that's installed
/// but unresponsive (Docker Desktop transitioning between states,
/// stale WSL2 socket bridge, swap-bound host) used to hang the
/// caller forever — pre-fix the SPA's setup wizard would spin
/// indefinitely on the docker-check screen. Now we kill the child
/// after the timeout and treat the result as `None` (unreachable)
/// so the wizard falls through to its "install Docker" branch.
fn try_docker_at(binary: &str) -> Option<DockerStatus> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let mut child = Command::new(binary)
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + DOCKER_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut buf = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut buf);
                }
                let version = buf.trim().to_owned();
                return Some(DockerStatus {
                    available: true,
                    version: if version.is_empty() {
                        None
                    } else {
                        Some(version)
                    },
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Daemon is unresponsive. Best-effort kill; if
                    // the kill itself blocks (rare on Linux/macOS,
                    // unusual on Windows), we drop the Child handle
                    // and let the OS reap it after our process
                    // exits — preferable to hanging the runtime.
                    tracing::warn!(
                        binary,
                        timeout_ms = DOCKER_PROBE_TIMEOUT.as_millis() as u64,
                        "docker info probe exceeded timeout — treating as unavailable",
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// How long [`try_docker_at`] waits for `docker info` to return
/// before declaring the daemon unreachable. Three seconds is a
/// generous budget for a healthy daemon (typical response is
/// 50-200 ms) and short enough that the SPA's setup wizard feels
/// responsive on a broken-Docker host.
const DOCKER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Probe whether Ollama is installed and runnable. Two-step check
/// so a stale symlink / permission-denied binary doesn't surface as
/// `available: true` and then crash the supervisor:
///
///   1. Run the path-resolution logic the supervisor uses for real
///      spawns (`NativeServiceController::discover_ollama`).
///   2. Shell out `<path> --version` and accept only an exit-0 reply.
///      Output is `ollama version is 0.1.43` on every release ≥0.1;
///      we strip the prefix to surface the bare version string.
///
/// Returns `{ available: false, version: None, path: None }` for any
/// failure — the wizard renders the install panel and the operator
/// gets a clear `brew install ollama` instruction instead of a
/// generic "supervisor crashed" later.
fn detect_ollama() -> OllamaStatus {
    use std::process::Command;
    let path = match NativeServiceController::discover_ollama() {
        Ok(p) => p,
        Err(_) => {
            return OllamaStatus {
                available: false,
                version: None,
                path: None,
            };
        }
    };
    let output = Command::new(&path).arg("--version").output();
    match output {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            // Strip the conventional `ollama version is ` prefix when
            // present so the SPA can render just the version number;
            // fall back to the raw string for forward-compat with
            // releases that change the format.
            let version = raw
                .strip_prefix("ollama version is ")
                .map(str::trim)
                .map(str::to_owned)
                .or(if raw.is_empty() { None } else { Some(raw) });
            OllamaStatus {
                available: true,
                version,
                path: Some(path.display().to_string()),
            }
        }
        _ => OllamaStatus {
            available: false,
            version: None,
            path: Some(path.display().to_string()),
        },
    }
}

/// `POST /api/admin/setup/dismiss` — mark the first-run wizard as
/// dismissed so `/api/ping` flips from `wizard` to `pong` and the
/// SPA's setup guard stops bouncing the operator back to /setup.
///
/// Idempotent. Audit-logged. The SPA's "Skip for now" button on the
/// backend step calls this immediately before navigating to /chat.
#[utoipa::path(
    post,
    path = "/api/admin/setup/dismiss",
    responses(
        (status = 200, description = "Wizard marked dismissed"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "setup"
)]
pub async fn dismiss_handler(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let now = chrono::Utc::now().timestamp();
    GeneralSettingsStore::new(&state.db)
        .dismiss_setup_wizard(now)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "setup_wizard",
        "dismiss",
        None,
        Some(&serde_json::json!({ "dismissed_at": now })),
    );
    Ok(StatusCode::OK)
}

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    match row.map(|u| u.role) {
        Some(UserRole::Controller) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "only a Controller can dismiss the setup wizard".into(),
        }),
    }
}

pub fn setup_preflight_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/setup/preflight", get(get_handler))
        .route("/api/admin/setup/dismiss", post(dismiss_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;

    async fn setup_controller_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Ctrl",
        }))
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["access_token"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn preflight_returns_docker_and_gpus_shape() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/setup/preflight")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Docker probe is a real shell-out — we don't assert what
        // the host has; just that the schema is right and either
        // shape (available true or false) is present.
        assert!(v["docker"].is_object());
        assert!(v["docker"]["available"].is_boolean());
        assert!(v["docker"]["version"].is_string() || v["docker"]["version"].is_null());
        assert!(v["gpus"].is_array());
        // Ollama detection added in Phase 14.G — the wizard's
        // Apple-Silicon panel gates on this. Like docker, it's a
        // real shell-out so we only assert schema, not host state.
        assert!(
            v["ollama"].is_object(),
            "preflight must carry an `ollama` object"
        );
        assert!(v["ollama"]["available"].is_boolean());
        // `version` and `path` are nullable when the binary isn't
        // present (or `--version` failed) — the SPA renders the
        // install panel in that case.
        assert!(v["ollama"]["version"].is_string() || v["ollama"]["version"].is_null());
        assert!(v["ollama"]["path"].is_string() || v["ollama"]["path"].is_null());
    }

    #[test]
    fn detect_ollama_returns_unavailable_with_no_binary_present() {
        // Sanity check on the helper itself rather than the route.
        // We can't ensure `ollama` is absent from the test runner's
        // PATH (CI machines might have it installed), but we CAN
        // assert that the function never panics + always returns a
        // well-shaped value. The serialization test above pins the
        // schema; this test pins the no-panic contract.
        let s = detect_ollama();
        // available is a bool either way; version/path are Option.
        // No assertion on the actual value — just that we got here.
        let _ = s.available;
        let _ = s.version;
        let _ = s.path;
    }

    #[tokio::test]
    async fn preflight_requires_auth() {
        let app = build_router(test_app_state());
        // No token at all.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/setup/preflight")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "expected 401/403, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn dismiss_marks_wizard_complete_and_flips_ping_to_pong() {
        use execlaw_core::general_settings::GeneralSettingsStore;
        let state = crate::routes::test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        // Pre-condition: ping says wizard (account exists, backend
        // doesn't, not dismissed).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"wizard");

        // Dismiss.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/setup/dismiss")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Store carries the timestamp.
        assert!(
            GeneralSettingsStore::new(&state.db)
                .wizard_dismissed()
                .unwrap()
        );

        // Ping now says pong.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"pong");
    }

    #[tokio::test]
    async fn dismiss_requires_auth() {
        let app = build_router(crate::routes::test_app_state());
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/setup/dismiss")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "expected 401/403, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn preflight_includes_disk_free_fields() {
        // Phase 14.D — wizard's disk-space preflight reads
        // `disk_free_bytes` + `disk_free_path` from this endpoint.
        // The probe is platform-native (`fs4`); on a working dev
        // host both fields populate. We assert the schema, not a
        // specific byte count.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/setup/preflight")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["disk_free_bytes"].is_number() || v["disk_free_bytes"].is_null(),
            "expected u64 or null disk_free_bytes; got {:?}",
            v["disk_free_bytes"]
        );
        assert!(
            v["disk_free_path"].is_string() || v["disk_free_path"].is_null(),
            "expected string or null disk_free_path; got {:?}",
            v["disk_free_path"]
        );
        if let Some(b) = v["disk_free_bytes"].as_u64() {
            // Sanity: a real probe on a dev host should return more
            // than 1 KB. If we get exactly 0, the fs4 probe lied or
            // the volume is genuinely full — either way worth a warn.
            assert!(
                b > 0,
                "disk_free_bytes should be positive on a working host"
            );
        }
    }

    #[tokio::test]
    async fn preflight_includes_cached_models_field() {
        // The wizard subtracts `cached_models[picked_id]` from the
        // disk-space requirement so an operator with the model
        // already downloaded doesn't see a false "not enough disk"
        // warning. The endpoint must always return a (possibly
        // empty) object so the frontend can read it without a
        // null-check branch.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/setup/preflight")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["cached_models"].is_object(),
            "cached_models must be an object (possibly empty); got {:?}",
            v["cached_models"]
        );
    }

    #[test]
    fn detect_cached_models_finds_models_in_a_seeded_cache_dir() {
        // Build a fake HF cache layout in a temp dir and verify the
        // pure enumerator (`cached_models_in`) returns the right
        // model_id → bytes map.
        //
        // The HF cache layout is:
        //   <cache>/hub/models--<owner>--<repo>/blobs/<sha256>
        // and the enumerator walks `models--*` dirs recursively.
        let tmp = tempfile::tempdir().unwrap();
        let hub = tmp.path().join("hub");
        let model_dir = hub.join("models--QuantTrio--Qwen3.5-27B-AWQ").join("blobs");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("part-1"), vec![0u8; 1024]).unwrap();
        std::fs::write(model_dir.join("part-2"), vec![0u8; 2048]).unwrap();

        // Also seed a non-models entry to confirm it's ignored.
        std::fs::create_dir_all(hub.join("ignored-other-thing")).unwrap();
        std::fs::write(hub.join("ignored-other-thing").join("x"), b"hello").unwrap();

        let map = cached_models_in(tmp.path());

        assert_eq!(
            map.get("QuantTrio/Qwen3.5-27B-AWQ"),
            Some(&3072),
            "enumerator must report the summed file bytes under the model dir; got map = {map:?}",
        );
        // Non-models entries don't appear.
        assert!(!map.contains_key("ignored-other-thing"));
    }

    #[test]
    fn detect_cached_models_returns_empty_when_cache_missing() {
        // Fresh install: the hub/ dir doesn't exist yet. The
        // enumerator must handle this gracefully — empty map, no
        // error. Without this guarantee, the wizard's first-run
        // path would 500 on every preflight.
        let tmp = tempfile::tempdir().unwrap();
        // Pass a path that's missing the `hub` subdir.
        let map = cached_models_in(tmp.path());
        assert!(map.is_empty());
    }

    #[test]
    fn detect_cached_models_skips_dirs_with_unexpected_naming() {
        // Defensive: a directory under `hub/` that doesn't follow
        // the `models--<owner>--<repo>` shape is silently skipped
        // — could be a partial download, an unrelated cache, or
        // future HF additions like `datasets--…`. We don't want a
        // single non-conforming entry to poison the rest of the
        // map.
        let tmp = tempfile::tempdir().unwrap();
        let hub = tmp.path().join("hub");
        std::fs::create_dir_all(hub.join("models--solo")).unwrap();
        std::fs::write(hub.join("models--solo").join("x"), b"abc").unwrap();
        std::fs::create_dir_all(hub.join("datasets--foo--bar")).unwrap();
        std::fs::create_dir_all(
            hub.join("models--Qwen--Qwen2.5-14B-Instruct-AWQ")
                .join("blobs"),
        )
        .unwrap();
        std::fs::write(
            hub.join("models--Qwen--Qwen2.5-14B-Instruct-AWQ")
                .join("blobs")
                .join("file"),
            vec![0u8; 512],
        )
        .unwrap();

        let map = cached_models_in(tmp.path());
        // Only the well-formed model is returned.
        assert_eq!(map.get("Qwen/Qwen2.5-14B-Instruct-AWQ"), Some(&512));
        assert_eq!(map.len(), 1, "unexpected entries: {map:?}");
    }

    #[test]
    fn detect_docker_returns_well_formed_status() {
        // Smoke: detect_docker doesn't panic and returns a coherent
        // shape regardless of whether docker is installed on the
        // test runner.
        let s = detect_docker();
        if s.available {
            // When available, version SHOULD be present (docker
            // info --format ServerVersion always returns something).
            // We tolerate empty version on edge-case dockerd builds.
            let _ = s.version;
        } else {
            assert!(s.version.is_none());
        }
    }
}
