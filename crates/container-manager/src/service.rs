//! Inference-backend service container lifecycle (Phase 12.B).
//!
//! `ServiceController` is the operator-facing primitive the
//! `BackendSupervisor` (server crate, Phase 12.C) drives to spawn,
//! stop, and health-check inference service containers (vLLM,
//! Whisper, Kokoro, etc.). The trait abstracts the underlying
//! container runtime so:
//!
//!   * Production wires `BollardServiceController` (real Docker).
//!   * Tests wire `MockServiceController` (deterministic in-memory
//!     state machine), which is the only way the supervisor's
//!     reconciliation logic gets meaningfully tested without a
//!     live Docker daemon.
//!
//! v1 scope: pull image, create + start + stop + inspect, plus an
//! HTTP health probe. GPU passthrough handles the locked-decision
//! NVIDIA case via `DeviceRequest`; Intel Arc / AMD passthrough lands
//! when those plugins do.

use crate::hardware::GpuVendor;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

/// Spec the supervisor hands to the controller. Self-contained — the
/// controller doesn't need to read any DB state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Container name used as a stable handle. The supervisor mints
    /// these as `execlaw-backend-{purpose}` so two managed backends
    /// for different purposes never collide.
    pub name: String,
    /// Full image reference, e.g. `vllm/vllm-openai:v0.6.2`.
    pub image: String,
    /// Arguments passed as the container's `Cmd`. The image's
    /// `ENTRYPOINT` is reused.
    pub args: Vec<String>,
    /// Environment variables as `(name, value)` pairs.
    pub env: Vec<(String, String)>,
    /// Operator-supplied GPU id; `None` runs CPU-only. The
    /// production controller resolves this against the host's
    /// detected hardware.
    ///
    /// Format depends on `gpu_vendor`:
    ///   * `Some(Nvidia)` — a small ordinal index (`"0"`, `"1"`)
    ///     matching nvidia-docker's `--gpus device=N` semantics, or
    ///     a CUDA UUID like `"GPU-…"`. The full PCI/PNP string from
    ///     `GpuId` is NOT acceptable to nvidia-docker.
    ///   * `Some(Intel)` — currently informational; Intel
    ///     passthrough binds `/dev/dri` (Linux) without consulting
    ///     this field.
    ///   * `Some(Amd)` / `None` — CPU-only spawn (no device passthrough).
    pub gpu_id: Option<String>,
    /// Vendor of the picked GPU, if any. Drives which container-runtime
    /// device passthrough strategy `BollardServiceController` uses.
    /// Stored in `model_spec_json` as the `gpu_vendor` string field
    /// (`"nvidia" | "intel" | "amd"`); rows that omit it fall through
    /// to "no GPU passthrough" so a misconfigured row can't fail
    /// `create_container` with a runtime error the operator can't
    /// diagnose.
    pub gpu_vendor: Option<GpuVendor>,
    /// Host directories to bind into the container. Used today for
    /// mounting the host-side HuggingFace model cache so vLLM
    /// reads pre-downloaded weights from disk instead of pulling
    /// from HF on every spawn. Each entry maps a host path to a
    /// container path with a read-only flag; `read_only=true` is
    /// strongly preferred for cache mounts since the host-side
    /// downloader is the single writer.
    pub mounts: Vec<HostMount>,
    /// Host port to bind the service on. Picked by the supervisor
    /// from a per-purpose pool to keep URLs stable across restarts.
    pub host_port: u16,
    /// Container port the service listens on internally.
    pub container_port: u16,
}

/// One bind-mount the supervisor wires into the container's
/// `HostConfig.binds`. We use `Binds` (simple `host:container[:ro]`
/// strings) rather than the newer `Mounts` API because Docker
/// Desktop on Windows handles the path translation transparently
/// when the host path lives under a Drive that's been added to
/// "File sharing" (the default for `C:\` on a fresh install).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    /// Absolute path on the host. Windows paths (`C:\Users\…`) are
    /// accepted as-is — bollard hands them to dockerd which does
    /// the translation.
    pub host_path: String,
    /// Absolute path inside the container.
    pub container_path: String,
    /// True for read-only mounts (recommended for cache mounts).
    pub read_only: bool,
}

/// Handle returned by `spawn`. The supervisor stores this so it can
/// stop / inspect later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceHandle {
    /// Docker container id (full hex string).
    pub container_id: String,
    /// Echo of the spec's `name` for human-readable log lines.
    pub name: String,
    /// The actual bound host port (echoes spec.host_port — the
    /// supervisor picks the port up front so the URL is stable).
    pub host_port: u16,
}

impl ServiceHandle {
    /// Loopback URL the runner uses to call this backend's HTTP
    /// API. The supervisor writes this back into `config_backends.endpoint`
    /// so a turn doesn't need to consult the controller.
    pub fn endpoint_url(&self, scheme: &str) -> String {
        format!("{scheme}://127.0.0.1:{}", self.host_port)
    }
}

/// Coarse-grained status the supervisor surfaces to the SPA. Finer
/// detail lives in the docker logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceStatus {
    /// Image isn't cached and a pull is in progress.
    Pulling,
    /// Container is created, possibly starting up, but the health
    /// probe hasn't succeeded yet.
    Starting,
    /// Health probe succeeded recently.
    Healthy,
    /// Container exited or the health probe has been failing
    /// repeatedly. `restart_count` is the supervisor's count of
    /// consecutive restart attempts since last health.
    CrashLooping { restart_count: u32 },
    /// Container is intentionally stopped (operator action or
    /// supervisor reconcile).
    Stopped,
    /// No container with this handle exists. Returned by `inspect`
    /// when the container has been removed externally.
    NotFound,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("container runtime: {0}")]
    Runtime(String),
    #[error("image pull failed: {0}")]
    Pull(String),
    #[error("health probe error: {0}")]
    Health(String),
    #[error("invalid service spec: {0}")]
    Invalid(String),
}

/// Operations the supervisor performs against the container runtime.
/// Implementations must be `Send + Sync` because the supervisor
/// shares one across its tokio task pool.
#[async_trait]
pub trait ServiceController: Send + Sync {
    /// Pull the image (no-op if cached), create the container, and
    /// start it. Returns the handle used for subsequent ops.
    async fn spawn(&self, spec: &ServiceSpec) -> Result<ServiceHandle, ServiceError>;

    /// Stop and remove a container previously spawned. Best-effort:
    /// a NotFound or already-stopped container resolves Ok.
    async fn stop(&self, handle: &ServiceHandle) -> Result<(), ServiceError>;

    /// Inspect the runtime state of a container.
    async fn inspect(&self, handle: &ServiceHandle) -> Result<ServiceStatus, ServiceError>;

    /// Probe an HTTP `/health` endpoint at `url`. Returns Ok(true)
    /// for 2xx, Ok(false) for connection-refused / non-2xx, Err for
    /// protocol-level failures (timeout, DNS, etc.). The 2-second
    /// timeout default is appropriate for loopback probes; remote
    /// callers should override at construction.
    async fn health_check(&self, url: &str) -> Result<bool, ServiceError>;

    /// Return the last `lines` log entries from the container,
    /// concatenated with newlines. Used by the supervisor to attach
    /// failure context to a CrashLooping alert + by the SPA's "view
    /// logs" affordance. Best-effort: callers must tolerate Ok(empty)
    /// for containers that haven't emitted anything yet, and Err
    /// only for protocol-level failures (Docker daemon unreachable).
    async fn tail_logs(
        &self,
        handle: &ServiceHandle,
        lines: usize,
    ) -> Result<String, ServiceError>;
}

// ---------------------------------------------------------------------------
// Bollard-backed production implementation
// ---------------------------------------------------------------------------

/// Real Docker controller via `bollard`. Constructed once at server
/// boot from `Docker::connect_with_local_defaults()`.
pub struct BollardServiceController {
    docker: bollard::Docker,
    health_timeout: Duration,
    /// Reqwest client kept around so each health probe doesn't
    /// allocate a new TLS pool. Loopback only in v1; rustls is
    /// pulled in via the workspace feature on `reqwest` but never
    /// exercised against an HTTPS endpoint.
    http: reqwest::Client,
}

impl BollardServiceController {
    /// Connect to the local Docker daemon. Fails immediately if
    /// the daemon socket isn't reachable, so the operator gets a
    /// clear startup error instead of a per-spawn surprise.
    pub fn connect() -> Result<Self, ServiceError> {
        let docker = bollard::Docker::connect_with_local_defaults()
            .map_err(|e| ServiceError::Runtime(format!("connect: {e}")))?;
        Self::with_docker(docker)
    }

    pub fn with_docker(docker: bollard::Docker) -> Result<Self, ServiceError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| ServiceError::Runtime(format!("reqwest client: {e}")))?;
        Ok(Self {
            docker,
            health_timeout: Duration::from_secs(2),
            http,
        })
    }
}

#[async_trait]
impl ServiceController for BollardServiceController {
    async fn spawn(&self, spec: &ServiceSpec) -> Result<ServiceHandle, ServiceError> {
        use bollard::container::{
            Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
            StopContainerOptions,
        };
        use bollard::image::CreateImageOptions;
        use bollard::secret::{
            DeviceRequest, HostConfig, HostConfigLogConfig, PortBinding,
        };
        use futures_util::StreamExt;
        use std::collections::HashMap;

        if spec.name.trim().is_empty() {
            return Err(ServiceError::Invalid("name must not be empty".into()));
        }
        if spec.image.trim().is_empty() {
            return Err(ServiceError::Invalid("image must not be empty".into()));
        }

        // --- 1. Remove any stale container with the same name. This
        // happens on a server restart while previous managed
        // containers were still running: bollard's
        // create_container errors with HTTP 409 on name conflict,
        // which would brick the spawn. Best-effort: stop + force-
        // remove. Errors are logged and swallowed — if the
        // container truly doesn't exist, both calls 404 and we
        // continue to the create.
        let _ = self
            .docker
            .stop_container(&spec.name, Some(StopContainerOptions { t: 5 }))
            .await;
        let _ = self
            .docker
            .remove_container(
                &spec.name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;

        // --- 2. Pull the image (no-op when cached, but Docker
        // ALWAYS does a manifest check against the registry so
        // even cached images take a few seconds — `:nightly` /
        // `:latest` tags can take 30s+ when fresh layers are
        // available). We log periodic progress so the supervisor
        // doesn't appear "silent" during long pulls; without this
        // an operator watching the SPA's Provisioning pill has no
        // signal that work is happening.
        let opts = CreateImageOptions {
            from_image: spec.image.clone(),
            ..Default::default()
        };
        let mut pull = self.docker.create_image(Some(opts), None, None);
        let mut last_log = std::time::Instant::now();
        let mut last_status = String::new();
        let mut event_count: u32 = 0;
        let pull_started = std::time::Instant::now();
        tracing::info!(
            image = %spec.image,
            container = %spec.name,
            "image pull started"
        );
        while let Some(ev) = pull.next().await {
            match ev {
                Ok(info) => {
                    event_count += 1;
                    // Log on status-string change (rare — "Pulling
                    // fs layer", "Downloading", "Extracting") OR
                    // every 5 seconds so a long download still
                    // emits a heartbeat.
                    if let Some(status) = info.status.as_deref() {
                        if status != last_status {
                            tracing::info!(
                                image = %spec.image,
                                layer = ?info.id,
                                status = %status,
                                event_count,
                                "image pull progress"
                            );
                            last_status = status.to_owned();
                            last_log = std::time::Instant::now();
                        } else if last_log.elapsed() >= std::time::Duration::from_secs(5) {
                            tracing::info!(
                                image = %spec.image,
                                layer = ?info.id,
                                status = %status,
                                progress = ?info.progress,
                                event_count,
                                "image pull heartbeat"
                            );
                            last_log = std::time::Instant::now();
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(image = %spec.image, "image pull failed: {e}");
                    return Err(ServiceError::Pull(e.to_string()));
                }
            }
        }
        tracing::info!(
            image = %spec.image,
            container = %spec.name,
            elapsed_secs = pull_started.elapsed().as_secs(),
            event_count,
            "image pull complete"
        );

        // --- 2. Build the container Config + HostConfig.
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        let key = format!("{}/tcp", spec.container_port);
        port_bindings.insert(
            key.clone(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".into()),
                host_port: Some(spec.host_port.to_string()),
            }]),
        );
        let mut exposed: HashMap<String, HashMap<(), ()>> = HashMap::new();
        exposed.insert(key, HashMap::new());

        // GPU passthrough — vendor-aware:
        //   * NVIDIA → DeviceRequest with the nvidia driver. Requires
        //     a small ordinal (`"0"`, `"1"`) or a CUDA UUID; the full
        //     `GpuId` string ("0x10de:PCI\VEN_10DE&DEV_…") that the
        //     SetupWizard used to send is NOT accepted and bricks
        //     create_container with HTTP 400.
        //   * Intel → bind /dev/dri devices on Linux (Docker Desktop
        //     on Windows + macOS has no device-passthrough surface
        //     for Intel Arc, so the spawn falls through to CPU mode
        //     and the inference container will run on CPU. The Intel
        //     plugins should refuse to start in that case rather
        //     than silently degrading.)
        //   * AMD / None → no passthrough (CPU-only).
        let mut device_requests: Option<Vec<DeviceRequest>> = None;
        let mut devices: Option<Vec<bollard::secret::DeviceMapping>> = None;
        match (spec.gpu_vendor, spec.gpu_id.as_deref()) {
            (Some(GpuVendor::Nvidia), Some(id)) => {
                device_requests = Some(vec![DeviceRequest {
                    driver: Some("nvidia".into()),
                    device_ids: Some(vec![id.to_owned()]),
                    capabilities: Some(vec![vec!["gpu".into()]]),
                    count: None,
                    options: None,
                }]);
            }
            (Some(GpuVendor::Nvidia), None) => {
                // "Any NVIDIA GPU" — the operator picked NVIDIA but
                // didn't pin a card. Pass count=-1 (== all) so docker
                // exposes every available card; the inference image
                // sees them via CUDA_VISIBLE_DEVICES inside.
                device_requests = Some(vec![DeviceRequest {
                    driver: Some("nvidia".into()),
                    device_ids: None,
                    capabilities: Some(vec![vec!["gpu".into()]]),
                    count: Some(-1),
                    options: None,
                }]);
            }
            (Some(GpuVendor::Intel), _) => {
                // Linux-only — Docker Desktop on Windows/macOS doesn't
                // forward /dev/dri to containers. We attempt the bind
                // unconditionally; on a host without /dev/dri the
                // bollard call fails with a clear "no such file"
                // error which the supervisor reports as CrashLooping
                // — the operator gets a real signal instead of a
                // silent CPU fallback that pretends to be GPU mode.
                devices = Some(vec![
                    bollard::secret::DeviceMapping {
                        path_on_host: Some("/dev/dri".into()),
                        path_in_container: Some("/dev/dri".into()),
                        cgroup_permissions: Some("rwm".into()),
                    },
                ]);
            }
            // AMD or no vendor → no device passthrough. The container
            // runs CPU-only; the inference image's startup script
            // decides whether that's acceptable.
            _ => {}
        }

        // Render `spec.mounts` into the bollard `binds` shape:
        // `"<host>:<container>[:ro]"`. We sanity-check the host
        // path exists on this side so a typo doesn't get to dockerd
        // (which would error 400 with a less-helpful message).
        // Read-only mounts get the `:ro` suffix; rw is the default.
        let binds: Vec<String> = spec
            .mounts
            .iter()
            .filter(|m| {
                let host = std::path::Path::new(&m.host_path);
                if !host.exists() {
                    tracing::warn!(
                        host_path = %m.host_path,
                        container_path = %m.container_path,
                        "mount host_path does not exist; skipping bind"
                    );
                    false
                } else {
                    true
                }
            })
            .map(|m| {
                if m.read_only {
                    format!("{}:{}:ro", m.host_path, m.container_path)
                } else {
                    format!("{}:{}", m.host_path, m.container_path)
                }
            })
            .collect();

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            device_requests,
            devices,
            binds: if binds.is_empty() { None } else { Some(binds) },
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".into()),
                config: Some(
                    [("max-size".into(), "10m".into()), ("max-file".into(), "3".into())]
                        .into_iter()
                        .collect(),
                ),
            }),
            ..Default::default()
        };

        let env: Vec<String> = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let cfg = Config {
            image: Some(spec.image.clone()),
            cmd: Some(spec.args.clone()),
            env: Some(env),
            exposed_ports: Some(exposed),
            host_config: Some(host_config),
            ..Default::default()
        };

        // --- 3. Create + start.
        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: spec.name.clone(),
                    platform: None,
                }),
                cfg,
            )
            .await
            .map_err(|e| ServiceError::Runtime(format!("create: {e}")))?;

        self.docker
            .start_container(&spec.name, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| ServiceError::Runtime(format!("start: {e}")))?;

        Ok(ServiceHandle {
            container_id: create.id,
            name: spec.name.clone(),
            host_port: spec.host_port,
        })
    }

    async fn stop(&self, handle: &ServiceHandle) -> Result<(), ServiceError> {
        use bollard::container::{RemoveContainerOptions, StopContainerOptions};

        // Stop, then remove. We swallow NotFound on remove so a
        // container that's already gone doesn't tip the supervisor.
        let _ = self
            .docker
            .stop_container(&handle.name, Some(StopContainerOptions { t: 10 }))
            .await;
        let _ = self
            .docker
            .remove_container(
                &handle.name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        Ok(())
    }

    async fn inspect(&self, handle: &ServiceHandle) -> Result<ServiceStatus, ServiceError> {
        use bollard::container::InspectContainerOptions;
        match self
            .docker
            .inspect_container(&handle.name, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => {
                let state = info.state.unwrap_or_default();
                let running = state.running.unwrap_or(false);
                let restart_count = info.restart_count.unwrap_or(0).max(0) as u32;
                let exit_code = state.exit_code.unwrap_or(0);
                if running {
                    // The HTTP probe (separate call) decides Healthy
                    // vs Starting; bollard alone can't tell us if
                    // the in-container service has finished
                    // bootstrapping.
                    Ok(ServiceStatus::Starting)
                } else if restart_count >= 3
                    || exit_code != 0
                    || state.dead.unwrap_or(false)
                {
                    Ok(ServiceStatus::CrashLooping { restart_count })
                } else {
                    Ok(ServiceStatus::Stopped)
                }
            }
            // bollard returns DockerResponseServerError 404 when a
            // container doesn't exist; treat that as NotFound so
            // the supervisor knows to re-spawn rather than retry.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(ServiceStatus::NotFound),
            Err(e) => Err(ServiceError::Runtime(format!("inspect: {e}"))),
        }
    }

    async fn health_check(&self, url: &str) -> Result<bool, ServiceError> {
        match self.http.get(url).timeout(self.health_timeout).send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            // Connection refused / timeout — the service hasn't come
            // up yet (or has died). NOT a protocol-level error; the
            // supervisor uses the false return to decide the status.
            Err(e) if e.is_connect() || e.is_timeout() => Ok(false),
            Err(e) => Err(ServiceError::Health(e.to_string())),
        }
    }

    async fn tail_logs(
        &self,
        handle: &ServiceHandle,
        lines: usize,
    ) -> Result<String, ServiceError> {
        use bollard::container::LogsOptions;
        use futures_util::StreamExt;

        let opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            // Bollard expects "all" or a numeric string for tail.
            tail: lines.to_string(),
            timestamps: false,
            follow: false,
            ..Default::default()
        };
        let mut stream = self.docker.logs(&handle.name, Some(opts));
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(chunk) => {
                    // bollard's LogOutput Display impl strips the
                    // 8-byte header docker prepends to each frame, so
                    // we can just push the rendered string. Each
                    // frame already carries its trailing newline.
                    out.push_str(&chunk.to_string());
                }
                Err(e) => {
                    // Container removed mid-read isn't fatal — return
                    // whatever we collected so far.
                    if out.is_empty() {
                        return Err(ServiceError::Runtime(format!(
                            "logs stream: {e}"
                        )));
                    }
                    break;
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// In-memory mock for tests
// ---------------------------------------------------------------------------

/// Deterministic mock for unit tests. Tracks spawn/stop calls and
/// lets tests inject status / health responses programmatically.
#[cfg(any(test, feature = "test-mock"))]
#[derive(Default)]
pub struct MockServiceController {
    inner: tokio::sync::Mutex<MockState>,
}

#[cfg(any(test, feature = "test-mock"))]
#[derive(Default)]
struct MockState {
    /// Containers the mock pretends are running, keyed by name.
    running: std::collections::HashMap<String, ServiceHandle>,
    /// Status the mock returns for any inspect call. Defaults to
    /// `Healthy` for any container in `running`, `NotFound`
    /// otherwise, but tests can pin a specific status.
    pinned_status: Option<ServiceStatus>,
    /// What `health_check` returns. Defaults to true.
    health_response: Option<Result<bool, String>>,
    /// What `spawn` returns. Defaults to a synthetic Ok handle.
    /// Tests pin this to simulate image-pull failures, name
    /// collisions, or other Docker errors.
    spawn_response: Option<Result<(), String>>,
    /// Spawn-call recorder for assertions.
    pub spawn_log: Vec<ServiceSpec>,
    /// Stop-call recorder for assertions.
    pub stop_log: Vec<ServiceHandle>,
    /// Per-container synthetic log output for tests that exercise
    /// the supervisor's "attach logs to alert" path.
    pub pinned_logs: std::collections::HashMap<String, String>,
}

#[cfg(any(test, feature = "test-mock"))]
impl MockServiceController {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn pin_status(&self, status: ServiceStatus) {
        self.inner.lock().await.pinned_status = Some(status);
    }

    pub async fn pin_health(&self, ok: bool) {
        self.inner.lock().await.health_response = Some(Ok(ok));
    }

    pub async fn pin_health_error(&self, msg: impl Into<String>) {
        self.inner.lock().await.health_response = Some(Err(msg.into()));
    }

    /// Force the next `spawn` (and every subsequent one until
    /// cleared) to return `ServiceError::Pull(msg)`. Tests use this
    /// to exercise the supervisor's spawn-failure branch without a
    /// real Docker daemon.
    pub async fn pin_spawn_pull_error(&self, msg: impl Into<String>) {
        self.inner.lock().await.spawn_response = Some(Err(msg.into()));
    }

    /// Drop a previously-pinned spawn response so subsequent calls
    /// fall through to the default success path. Used by tests that
    /// simulate "broken → fixed" recovery flows.
    pub async fn clear_spawn_response(&self) {
        self.inner.lock().await.spawn_response = None;
    }

    /// Pin a synthetic log payload for the given container name.
    /// Tests use this to verify that supervisor failure paths
    /// attach the captured log tail to the resulting alert.
    pub async fn pin_logs(&self, container_name: impl Into<String>, body: impl Into<String>) {
        self.inner
            .lock()
            .await
            .pinned_logs
            .insert(container_name.into(), body.into());
    }

    pub async fn spawn_count(&self) -> usize {
        self.inner.lock().await.spawn_log.len()
    }

    pub async fn stop_count(&self) -> usize {
        self.inner.lock().await.stop_log.len()
    }

    pub async fn last_spawn(&self) -> Option<ServiceSpec> {
        self.inner.lock().await.spawn_log.last().cloned()
    }
}

#[cfg(any(test, feature = "test-mock"))]
#[async_trait]
impl ServiceController for MockServiceController {
    async fn spawn(&self, spec: &ServiceSpec) -> Result<ServiceHandle, ServiceError> {
        let mut state = self.inner.lock().await;
        state.spawn_log.push(spec.clone());
        if let Some(Err(msg)) = state.spawn_response.clone() {
            return Err(ServiceError::Pull(msg));
        }
        let handle = ServiceHandle {
            container_id: format!("mock-{}", spec.name),
            name: spec.name.clone(),
            host_port: spec.host_port,
        };
        state.running.insert(spec.name.clone(), handle.clone());
        Ok(handle)
    }

    async fn stop(&self, handle: &ServiceHandle) -> Result<(), ServiceError> {
        let mut state = self.inner.lock().await;
        state.running.remove(&handle.name);
        state.stop_log.push(handle.clone());
        Ok(())
    }

    async fn inspect(&self, handle: &ServiceHandle) -> Result<ServiceStatus, ServiceError> {
        let state = self.inner.lock().await;
        if let Some(s) = state.pinned_status.clone() {
            return Ok(s);
        }
        if state.running.contains_key(&handle.name) {
            Ok(ServiceStatus::Healthy)
        } else {
            Ok(ServiceStatus::NotFound)
        }
    }

    async fn health_check(&self, _url: &str) -> Result<bool, ServiceError> {
        let state = self.inner.lock().await;
        match state.health_response.clone() {
            Some(Ok(b)) => Ok(b),
            Some(Err(e)) => Err(ServiceError::Health(e)),
            None => Ok(true),
        }
    }

    async fn tail_logs(
        &self,
        handle: &ServiceHandle,
        _lines: usize,
    ) -> Result<String, ServiceError> {
        let state = self.inner.lock().await;
        // Tests can pin a synthetic log payload via `pin_logs`;
        // otherwise return an empty string so the supervisor's
        // "best-effort attach logs" path stays exercised without
        // forcing every test to set up fixtures.
        match state.pinned_logs.get(&handle.name) {
            Some(s) => Ok(s.clone()),
            None => Ok(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_spec() -> ServiceSpec {
        ServiceSpec {
            name: "execlaw-backend-Standard".into(),
            image: "vllm/vllm-openai:v0.6.2".into(),
            args: vec!["--model".into(), "Qwen3.5-27B-AWQ".into()],
            env: vec![("HF_HOME".into(), "/cache".into())],
            gpu_id: Some("0".into()),
            gpu_vendor: Some(GpuVendor::Nvidia),
            mounts: Vec::new(),
            host_port: 8001,
            container_port: 8000,
        }
    }

    #[test]
    fn endpoint_url_uses_host_port_and_loopback() {
        let h = ServiceHandle {
            container_id: "abc".into(),
            name: "n".into(),
            host_port: 8123,
        };
        assert_eq!(h.endpoint_url("http"), "http://127.0.0.1:8123");
    }

    #[tokio::test]
    async fn mock_spawn_records_call_and_records_handle() {
        let mock = MockServiceController::new();
        let h = mock.spawn(&fixture_spec()).await.unwrap();
        assert_eq!(h.host_port, 8001);
        assert_eq!(h.name, "execlaw-backend-Standard");
        assert_eq!(mock.spawn_count().await, 1);
    }

    #[tokio::test]
    async fn mock_inspect_returns_healthy_for_running_container() {
        let mock = MockServiceController::new();
        let h = mock.spawn(&fixture_spec()).await.unwrap();
        assert_eq!(mock.inspect(&h).await.unwrap(), ServiceStatus::Healthy);
    }

    #[tokio::test]
    async fn mock_inspect_returns_not_found_after_stop() {
        let mock = MockServiceController::new();
        let h = mock.spawn(&fixture_spec()).await.unwrap();
        mock.stop(&h).await.unwrap();
        assert_eq!(mock.inspect(&h).await.unwrap(), ServiceStatus::NotFound);
        assert_eq!(mock.stop_count().await, 1);
    }

    #[tokio::test]
    async fn mock_pinned_status_overrides_running_state() {
        // Lets supervisor tests force a CrashLooping observation
        // even though the mock has the container listed as running.
        let mock = MockServiceController::new();
        let h = mock.spawn(&fixture_spec()).await.unwrap();
        mock.pin_status(ServiceStatus::CrashLooping { restart_count: 4 })
            .await;
        match mock.inspect(&h).await.unwrap() {
            ServiceStatus::CrashLooping { restart_count } => {
                assert_eq!(restart_count, 4);
            }
            other => panic!("expected CrashLooping, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_health_check_defaults_true_and_can_pin_false() {
        let mock = MockServiceController::new();
        assert!(mock.health_check("http://anything").await.unwrap());
        mock.pin_health(false).await;
        assert!(!mock.health_check("http://anything").await.unwrap());
    }

    #[tokio::test]
    async fn mock_health_check_can_pin_an_error() {
        let mock = MockServiceController::new();
        mock.pin_health_error("dns lookup failed").await;
        let err = mock.health_check("http://x").await.unwrap_err();
        assert!(matches!(err, ServiceError::Health(_)));
    }

    #[tokio::test]
    async fn mock_spawn_pin_pull_error_returns_service_error_pull() {
        // Closure for Phase 12 audit gap #4: the BollardServiceController's
        // `ServiceError::Pull` branch couldn't be exercised in tests.
        // The mock's `pin_spawn_pull_error` simulates the same shape so
        // the BackendSupervisor's spawn-failure handling has coverage.
        let mock = MockServiceController::new();
        mock.pin_spawn_pull_error("registry returned 404").await;
        let err = mock.spawn(&fixture_spec()).await.unwrap_err();
        match err {
            ServiceError::Pull(msg) => {
                assert!(msg.contains("registry returned 404"));
            }
            other => panic!("expected Pull, got {other:?}"),
        }
        // The spawn was still recorded — tests that count attempts
        // see a real attempt rather than a silently-skipped one.
        assert_eq!(mock.spawn_count().await, 1);
    }
}
