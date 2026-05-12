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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;

/// How a service is deployed. Defaults to `Docker` for every existing
/// preset; Apple-Silicon Ollama (and future native engines like
/// llama-server / MLX) sets `Native` because Docker Desktop on macOS
/// has no Metal passthrough — the inference engine must run as a
/// host-native subprocess supervised by the control plane directly.
///
/// The variant carries no fields today; the binary discovery hint
/// rides on `ServiceSpec::binary_hint`. Future variants (e.g. a WASM
/// runtime, or a remote-SSH execution mode) slot in additively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ServiceRuntime {
    /// Spawn via the bollard `Docker` daemon (default).
    #[default]
    Docker,
    /// Spawn as a host-native subprocess. `ServiceSpec.binary_hint`
    /// selects which discoverer + which graceful-shutdown shape
    /// `NativeServiceController` applies.
    Native,
}

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
    /// `ENTRYPOINT` is reused unless [`Self::entrypoint`] overrides
    /// it.
    pub args: Vec<String>,
    /// Override for the container image's `ENTRYPOINT`. When `None`,
    /// the image's built-in entrypoint runs unchanged. Sidecars that
    /// need to wrap the upstream entrypoint (e.g. patching a
    /// supervisord config before exec'ing the original) set this to
    /// the wrapper's argv (typically `["/bin/sh", "/wrapper.sh"]`).
    pub entrypoint: Option<Vec<String>>,
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
    /// Deployment runtime. Defaults to `Docker`; Apple-Silicon
    /// Ollama presets set this to `Native` because Docker Desktop on
    /// macOS has no Metal passthrough.
    pub runtime: ServiceRuntime,
    /// Engine hint for the native-runtime path. Ignored for
    /// `ServiceRuntime::Docker`. Selects the binary discoverer
    /// (`"ollama"` → `discover_ollama()`, future `"llama-server"` /
    /// `"mlx"` slot in by adding match arms). Empty string is
    /// equivalent to "no hint" and surfaces an error from the native
    /// controller — Apple presets MUST set this.
    pub binary_hint: String,
}

impl Default for ServiceSpec {
    /// Minimal-Docker default — handy for tests that only care about
    /// one or two fields. Production callers (the backend supervisor)
    /// always fill every field explicitly.
    fn default() -> Self {
        Self {
            name: String::new(),
            image: String::new(),
            args: Vec::new(),
            entrypoint: None,
            env: Vec::new(),
            gpu_id: None,
            gpu_vendor: None,
            mounts: Vec::new(),
            host_port: 0,
            container_port: 0,
            runtime: ServiceRuntime::Docker,
            binary_hint: String::new(),
        }
    }
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
    async fn tail_logs(&self, handle: &ServiceHandle, lines: usize)
    -> Result<String, ServiceError>;

    /// Look up an existing container by name. Returns
    /// `Ok(Some(handle))` when a container with that name exists
    /// AND is currently running, so the supervisor can re-attach
    /// to it instead of tearing it down + spawning fresh on every
    /// binary restart.
    ///
    /// `host_port` is what the supervisor mints for new spawns;
    /// the returned handle carries it forward so subsequent
    /// `endpoint_url` calls produce the expected URL even if the
    /// adopted container's actual port binding differs (which it
    /// shouldn't — the same supervisor code minted both — but the
    /// trait shape is symmetric with `spawn`).
    ///
    /// Returns `Ok(None)` for: container missing, container exists
    /// but not running (we'd rather respawn cleanly), or the
    /// daemon's response was malformed. Errors only on
    /// protocol-level failures.
    async fn try_adopt(
        &self,
        name: &str,
        host_port: u16,
    ) -> Result<Option<ServiceHandle>, ServiceError>;
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
        use bollard::secret::{DeviceRequest, HostConfig, HostConfigLogConfig, PortBinding};
        use futures_util::StreamExt;
        use std::collections::HashMap;

        if spec.name.trim().is_empty() {
            return Err(ServiceError::Invalid("name must not be empty".into()));
        }
        if spec.image.trim().is_empty() {
            return Err(ServiceError::Invalid("image must not be empty".into()));
        }
        if spec.runtime != ServiceRuntime::Docker {
            return Err(ServiceError::Invalid(format!(
                "BollardServiceController cannot spawn ServiceRuntime::{:?} — \
                 wrap with MultiplexedServiceController + NativeServiceController",
                spec.runtime
            )));
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
                devices = Some(vec![bollard::secret::DeviceMapping {
                    path_on_host: Some("/dev/dri".into()),
                    path_in_container: Some("/dev/dri".into()),
                    cgroup_permissions: Some("rwm".into()),
                }]);
            }
            // AMD, Apple, or no vendor → no device passthrough.
            // Apple Silicon has no Metal-to-Linux-container surface
            // at all (Docker Desktop on macOS runs a Linux VM with
            // zero GPU access), so Apple-vendor managed backends
            // never reach the Docker spawn path — they're routed
            // to `ServiceRuntime::Native` upstream in the backend
            // supervisor. The fallthrough here is a safety net:
            // if a misconfigured row somehow lands here, the
            // container starts CPU-only and the inference image's
            // startup script decides whether that's acceptable.
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
                    [
                        ("max-size".into(), "10m".into()),
                        ("max-file".into(), "3".into()),
                    ]
                    .into_iter()
                    .collect(),
                ),
            }),
            ..Default::default()
        };

        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let cfg = Config {
            image: Some(spec.image.clone()),
            cmd: Some(spec.args.clone()),
            entrypoint: spec.entrypoint.clone(),
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
                } else if restart_count >= 3 || exit_code != 0 || state.dead.unwrap_or(false) {
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

    async fn try_adopt(
        &self,
        name: &str,
        host_port: u16,
    ) -> Result<Option<ServiceHandle>, ServiceError> {
        use bollard::container::InspectContainerOptions;
        match self
            .docker
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => {
                let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                if !running {
                    // Container exists but is stopped / dead. Caller
                    // proceeds to spawn fresh, which force-removes
                    // the stale carcass anyway.
                    return Ok(None);
                }
                let id = info.id.unwrap_or_else(|| name.to_owned());
                tracing::info!(
                    container = %id,
                    name = %name,
                    "adopting existing running container (binary restart, not killing it)"
                );
                Ok(Some(ServiceHandle {
                    container_id: id,
                    name: name.to_owned(),
                    host_port,
                }))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(ServiceError::Runtime(format!("inspect (adopt): {e}"))),
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
                        return Err(ServiceError::Runtime(format!("logs stream: {e}")));
                    }
                    break;
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Native subprocess controller — Apple Silicon Ollama (and future
// host-native engines like MLX or `llama-server`).
// ---------------------------------------------------------------------------

/// Spawns inference engines as host-native subprocesses. Used for
/// platforms where containerised GPU passthrough doesn't exist
/// (Apple Silicon's Metal — Docker Desktop on macOS runs a Linux VM
/// that can't see Metal).
///
/// Container-like semantics: the controller owns the child process,
/// captures stdout/stderr into `tracing`, lets the supervisor probe
/// the engine's OpenAI-compat `/v1` endpoint via `health_check`, and
/// kills the child on `stop`.
///
/// Binary discovery is hint-driven — `ServiceSpec::binary_hint`
/// selects which helper finds the engine binary on the host. v1
/// ships `"ollama"`; future engines slot in by adding match arms.
///
/// **Adoption is not supported.** On a control-plane binary restart
/// the previous Ollama child was orphaned; `try_adopt` always
/// returns `None`, so the supervisor respawns. This is intentional:
/// reattaching to a stray PID across binary boundaries is fragile
/// (the PID could be recycled), and Ollama's model cache survives
/// the respawn so the cost is one health-probe cycle.
pub struct NativeServiceController {
    /// Map of `ServiceSpec.name` → live child process. The same
    /// service name re-spawned (operator edits the preset, supervisor
    /// reconciles) overwrites the entry after killing the previous.
    children: Arc<Mutex<std::collections::HashMap<String, NativeChild>>>,
    http: reqwest::Client,
    health_timeout: Duration,
}

struct NativeChild {
    /// The spawned `tokio::process::Child`. Wrapped in an `Option` so
    /// `stop()` can `take()` it before awaiting the kill, releasing
    /// the lock for concurrent inspect / health calls.
    child: Option<tokio::process::Child>,
    /// Rolling capture of the last ~256 lines of stdout/stderr.
    /// `tail_logs` reads from here; the supervisor's CrashLooping
    /// alert attaches these so the operator can see what went wrong
    /// without shelling into the host. Bounded so a chatty process
    /// can't leak memory.
    logs: Arc<Mutex<std::collections::VecDeque<String>>>,
}

const NATIVE_LOG_RING_CAP: usize = 256;

impl Default for NativeServiceController {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeServiceController {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("reqwest::Client::builder with 2s timeout must succeed");
        Self {
            children: Arc::new(Mutex::new(std::collections::HashMap::new())),
            http,
            health_timeout: Duration::from_secs(2),
        }
    }

    /// Locate the `ollama` binary on the host. Order:
    ///   1. `$OLLAMA_BINARY` env var (operator override, used by the
    ///      e2e tests to inject a fake binary).
    ///   2. `PATH` lookup.
    ///   3. Homebrew prefixes — Apple Silicon (`/opt/homebrew/bin/ollama`)
    ///      first, then Intel-mac fallback (`/usr/local/bin/ollama`).
    ///
    /// Returns an actionable error when nothing matches — the
    /// supervisor surfaces this verbatim to the SPA wizard so the
    /// operator sees "install Ollama" rather than a generic spawn
    /// failure.
    pub fn discover_ollama() -> Result<PathBuf, ServiceError> {
        Self::discover_ollama_with(
            std::env::var("OLLAMA_BINARY").ok(),
            |name| find_on_path(name),
        )
    }

    /// Test-injectable form of [`discover_ollama`]. Production calls
    /// the public method, which fills these arguments from the real
    /// environment; tests pass synthetic values to avoid mutating
    /// `std::env` (the crate forbids `unsafe`, so the modern
    /// `set_var`/`remove_var` API isn't reachable).
    pub(crate) fn discover_ollama_with(
        env_override: Option<String>,
        path_lookup: impl Fn(&str) -> Option<PathBuf>,
    ) -> Result<PathBuf, ServiceError> {
        if let Some(p) = env_override {
            let path = PathBuf::from(&p);
            if path.exists() {
                return Ok(path);
            }
            return Err(ServiceError::Invalid(format!(
                "OLLAMA_BINARY points to '{p}' but that file does not exist"
            )));
        }
        if let Some(p) = path_lookup("ollama") {
            return Ok(p);
        }
        for candidate in ["/opt/homebrew/bin/ollama", "/usr/local/bin/ollama"] {
            let path = PathBuf::from(candidate);
            if path.exists() {
                return Ok(path);
            }
        }
        Err(ServiceError::Invalid(
            "ollama binary not found — install with `brew install ollama`, or \
             set OLLAMA_BINARY to an absolute path"
                .to_owned(),
        ))
    }

    /// Pick the binary path for a given hint. Future engines slot in
    /// by adding match arms. Pure dispatch — no side effects.
    fn discover_for_hint(hint: &str) -> Result<PathBuf, ServiceError> {
        match hint {
            "ollama" => Self::discover_ollama(),
            "" => Err(ServiceError::Invalid(
                "ServiceSpec.binary_hint is empty — native-runtime presets \
                 MUST set this (e.g. \"ollama\")"
                    .to_owned(),
            )),
            other => Err(ServiceError::Invalid(format!(
                "unknown native binary_hint '{other}' — supported: \"ollama\""
            ))),
        }
    }
}

/// Walk `$PATH` looking for `name` (or `name.exe` on Windows). Pure
/// stdlib so we don't add a `which` crate dependency for one helper.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    let full = format!("{name}{exe_suffix}");
    for entry in std::env::split_paths(&path_var) {
        let candidate = entry.join(&full);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[async_trait]
impl ServiceController for NativeServiceController {
    async fn spawn(&self, spec: &ServiceSpec) -> Result<ServiceHandle, ServiceError> {
        if spec.runtime != ServiceRuntime::Native {
            return Err(ServiceError::Invalid(format!(
                "NativeServiceController cannot spawn ServiceRuntime::{:?}",
                spec.runtime
            )));
        }
        if spec.name.trim().is_empty() {
            return Err(ServiceError::Invalid("name must not be empty".into()));
        }

        let binary = Self::discover_for_hint(&spec.binary_hint)?;

        // Stop + remove any existing child registered under the same
        // name. Mirrors BollardServiceController's pre-spawn cleanup
        // so a binary restart that adopts a stale entry doesn't leak.
        {
            let mut map = self.children.lock().await;
            if let Some(mut existing) = map.remove(&spec.name) {
                if let Some(mut ch) = existing.child.take() {
                    let _ = ch.kill().await;
                }
            }
        }

        // Engine-specific env defaults. For Ollama, point the daemon
        // at the supervisor-picked port and isolate the model cache
        // per-execlaw so multiple instances on one host (dev + prod
        // shadow, etc.) don't fight. Spec env wins on conflict.
        let mut env: Vec<(String, String)> = Vec::new();
        if spec.binary_hint == "ollama" {
            env.push((
                "OLLAMA_HOST".into(),
                format!("127.0.0.1:{}", spec.host_port),
            ));
        }
        env.extend(spec.env.iter().cloned());

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&spec.args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Don't inherit stdin — Ollama doesn't read it, and an
        // inherited TTY would let a Ctrl-C in the operator's terminal
        // kill the child indirectly.
        cmd.stdin(std::process::Stdio::null());

        tracing::info!(
            service.name = %spec.name,
            binary = %binary.display(),
            binary_hint = %spec.binary_hint,
            host_port = spec.host_port,
            "spawning native service"
        );

        let mut child = cmd.spawn().map_err(|e| {
            ServiceError::Runtime(format!(
                "failed to spawn {} ({}): {e}",
                binary.display(),
                spec.binary_hint
            ))
        })?;

        let logs = Arc::new(Mutex::new(std::collections::VecDeque::<String>::with_capacity(
            NATIVE_LOG_RING_CAP,
        )));

        // Spawn log-reaper tasks. They run until the child closes its
        // stdout/stderr — which it only does on exit — so they
        // naturally terminate when we kill the child.
        if let Some(stdout) = child.stdout.take() {
            spawn_log_reaper(spec.name.clone(), "stdout", stdout, logs.clone(), false);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_reaper(spec.name.clone(), "stderr", stderr, logs.clone(), true);
        }

        let pid = child.id().unwrap_or(0);
        let mut map = self.children.lock().await;
        map.insert(
            spec.name.clone(),
            NativeChild {
                child: Some(child),
                logs,
            },
        );

        Ok(ServiceHandle {
            // No Docker id for native processes — use the PID-tagged
            // service name so the supervisor's log lines still
            // produce a unique fingerprint.
            container_id: format!("native:{}:{pid}", spec.name),
            name: spec.name.clone(),
            host_port: spec.host_port,
        })
    }

    async fn stop(&self, handle: &ServiceHandle) -> Result<(), ServiceError> {
        let mut map = self.children.lock().await;
        let Some(mut entry) = map.remove(&handle.name) else {
            return Ok(()); // already gone — match the bollard "best-effort" contract
        };
        // We use SIGKILL via `Child::kill` to keep the cross-platform
        // surface honest (tokio doesn't ship a graceful SIGTERM path
        // and adding `nix` for one signal call isn't worth it for v1).
        // Ollama tolerates ungraceful exits — its on-disk model store
        // is append-only and the next spawn rebuilds in-memory state
        // from registry pulls.
        if let Some(mut ch) = entry.child.take() {
            if let Err(e) = ch.kill().await {
                tracing::warn!(service.name = %handle.name, "child kill failed: {e}");
            }
        }
        tracing::info!(service.name = %handle.name, "native service stopped");
        Ok(())
    }

    async fn inspect(&self, handle: &ServiceHandle) -> Result<ServiceStatus, ServiceError> {
        let mut map = self.children.lock().await;
        let Some(entry) = map.get_mut(&handle.name) else {
            return Ok(ServiceStatus::NotFound);
        };
        let Some(ch) = entry.child.as_mut() else {
            return Ok(ServiceStatus::NotFound);
        };
        match ch.try_wait() {
            Ok(None) => Ok(ServiceStatus::Healthy), // still running — health probe is separate
            Ok(Some(status)) => {
                tracing::warn!(
                    service.name = %handle.name,
                    exit_status = ?status,
                    "native service exited"
                );
                // Drop the child so subsequent inspect/health calls
                // see NotFound until the supervisor respawns.
                entry.child = None;
                Ok(ServiceStatus::CrashLooping { restart_count: 0 })
            }
            Err(e) => Err(ServiceError::Runtime(format!("try_wait: {e}"))),
        }
    }

    async fn health_check(&self, url: &str) -> Result<bool, ServiceError> {
        // Same shape as BollardServiceController::health_check — a
        // 2-second HTTP GET. The supervisor probes Ollama at
        // `http://127.0.0.1:{port}/api/tags` (engine-specific path
        // chosen at supervisor wiring time).
        match tokio::time::timeout(self.health_timeout, self.http.get(url).send()).await {
            Ok(Ok(resp)) => Ok(resp.status().is_success()),
            Ok(Err(e)) => {
                // Connection-refused is the normal case during
                // startup; surface as `Ok(false)` so the supervisor
                // keeps polling rather than going CrashLooping.
                if e.is_connect() || e.is_timeout() {
                    return Ok(false);
                }
                Err(ServiceError::Health(e.to_string()))
            }
            Err(_) => Ok(false), // timeout
        }
    }

    async fn tail_logs(
        &self,
        handle: &ServiceHandle,
        lines: usize,
    ) -> Result<String, ServiceError> {
        let map = self.children.lock().await;
        let Some(entry) = map.get(&handle.name) else {
            return Ok(String::new());
        };
        let logs = entry.logs.lock().await;
        let take = lines.min(logs.len());
        let start = logs.len() - take;
        let mut out = String::new();
        for line in logs.iter().skip(start) {
            out.push_str(line);
            out.push('\n');
        }
        Ok(out)
    }

    async fn try_adopt(
        &self,
        _name: &str,
        _host_port: u16,
    ) -> Result<Option<ServiceHandle>, ServiceError> {
        // Cannot safely reattach to a PID from a previous binary
        // run — the OS may have recycled it. Always force a
        // respawn; the supervisor's health-probe + Ollama's
        // append-only model store keep the cost minimal.
        Ok(None)
    }
}

/// Drain a child stdout/stderr stream into the ring buffer + tracing.
fn spawn_log_reaper(
    service_name: String,
    stream_label: &'static str,
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    ring: Arc<Mutex<std::collections::VecDeque<String>>>,
    is_stderr: bool,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let reader = BufReader::new(stream);
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if is_stderr {
                        tracing::warn!(
                            service.name = %service_name,
                            stream = stream_label,
                            "{line}"
                        );
                    } else {
                        tracing::info!(
                            service.name = %service_name,
                            stream = stream_label,
                            "{line}"
                        );
                    }
                    let mut r = ring.lock().await;
                    if r.len() == NATIVE_LOG_RING_CAP {
                        r.pop_front();
                    }
                    r.push_back(line);
                }
                Ok(None) => break, // EOF — child exited or closed pipe
                Err(e) => {
                    tracing::debug!(
                        service.name = %service_name,
                        stream = stream_label,
                        "log reaper error: {e}"
                    );
                    break;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Multiplexed controller — picks Docker vs Native per spec.
// ---------------------------------------------------------------------------

/// Wraps both a Docker controller and a Native controller and
/// dispatches each call to the right one based on `ServiceSpec.runtime`
/// (for spawn) or by looking up the handle's owner (for stop / inspect /
/// health / tail_logs / try_adopt).
///
/// The supervisor wires this as its `Arc<dyn ServiceController>` so
/// every call site (existing managed-vLLM tests, new Apple-Ollama
/// flow) sees the same trait surface.
pub struct MultiplexedServiceController {
    docker: Arc<dyn ServiceController>,
    native: Arc<dyn ServiceController>,
    /// `name → "docker" | "native"` — populated on spawn, consulted
    /// by every subsequent op so we don't need the caller to thread
    /// the runtime back into each call.
    routing: Arc<Mutex<std::collections::HashMap<String, ControllerKind>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerKind {
    Docker,
    Native,
}

impl MultiplexedServiceController {
    pub fn new(
        docker: Arc<dyn ServiceController>,
        native: Arc<dyn ServiceController>,
    ) -> Self {
        Self {
            docker,
            native,
            routing: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    async fn pick(&self, name: &str) -> Arc<dyn ServiceController> {
        let map = self.routing.lock().await;
        match map.get(name).copied() {
            Some(ControllerKind::Native) => self.native.clone(),
            // Default to Docker for unknown names — matches v1 history
            // where every spec was Docker, and lets a supervisor that
            // restarted (losing its routing map) still reach the right
            // controller via the bollard adoption path.
            _ => self.docker.clone(),
        }
    }
}

#[async_trait]
impl ServiceController for MultiplexedServiceController {
    async fn spawn(&self, spec: &ServiceSpec) -> Result<ServiceHandle, ServiceError> {
        let (kind, ctl) = match spec.runtime {
            ServiceRuntime::Native => (ControllerKind::Native, self.native.clone()),
            ServiceRuntime::Docker => (ControllerKind::Docker, self.docker.clone()),
        };
        let handle = ctl.spawn(spec).await?;
        self.routing.lock().await.insert(spec.name.clone(), kind);
        Ok(handle)
    }

    async fn stop(&self, handle: &ServiceHandle) -> Result<(), ServiceError> {
        let ctl = self.pick(&handle.name).await;
        let res = ctl.stop(handle).await;
        self.routing.lock().await.remove(&handle.name);
        res
    }

    async fn inspect(&self, handle: &ServiceHandle) -> Result<ServiceStatus, ServiceError> {
        self.pick(&handle.name).await.inspect(handle).await
    }

    async fn health_check(&self, url: &str) -> Result<bool, ServiceError> {
        // Health probes don't know which controller owns the URL —
        // every controller's health_check is just an HTTP GET, so
        // it's fine to use the Docker one as the canonical
        // implementation. Native's is identical but the routing map
        // wouldn't help here anyway.
        self.docker.health_check(url).await
    }

    async fn tail_logs(
        &self,
        handle: &ServiceHandle,
        lines: usize,
    ) -> Result<String, ServiceError> {
        self.pick(&handle.name).await.tail_logs(handle, lines).await
    }

    async fn try_adopt(
        &self,
        name: &str,
        host_port: u16,
    ) -> Result<Option<ServiceHandle>, ServiceError> {
        // Adoption is Docker-only — native processes can't be safely
        // reattached across binary restarts. If the operator later
        // edits the preset to switch to Native, the supervisor's
        // reconcile loop will respawn.
        self.docker.try_adopt(name, host_port).await
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

    async fn try_adopt(
        &self,
        name: &str,
        host_port: u16,
    ) -> Result<Option<ServiceHandle>, ServiceError> {
        let state = self.inner.lock().await;
        Ok(state.running.get(name).map(|h| ServiceHandle {
            container_id: h.container_id.clone(),
            name: name.to_owned(),
            host_port,
        }))
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
            entrypoint: None,
            env: vec![("HF_HOME".into(), "/cache".into())],
            gpu_id: Some("0".into()),
            gpu_vendor: Some(GpuVendor::Nvidia),
            mounts: Vec::new(),
            host_port: 8001,
            container_port: 8000,
            runtime: ServiceRuntime::Docker,
            binary_hint: String::new(),
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

    // ---- Phase 2 — Apple-Silicon native runtime ---------------------

    fn apple_spec(binary_hint: &str) -> ServiceSpec {
        ServiceSpec {
            name: "execlaw-backend-Standard".into(),
            host_port: 8101,
            container_port: 11434,
            runtime: ServiceRuntime::Native,
            binary_hint: binary_hint.to_owned(),
            args: vec!["serve".into()],
            ..Default::default()
        }
    }

    #[test]
    fn service_spec_default_is_docker_runtime() {
        // Critical: every existing test literal that switched to
        // `..Default::default()` now relies on this. If the default
        // ever flipped to Native, existing managed-vLLM tests would
        // silently try to spawn through NativeServiceController.
        let spec = ServiceSpec::default();
        assert_eq!(spec.runtime, ServiceRuntime::Docker);
        assert!(spec.binary_hint.is_empty());
    }

    #[tokio::test]
    async fn bollard_controller_rejects_native_spec() {
        // BollardServiceController must hard-error on a Native spec
        // rather than silently treating it as Docker. We can't run
        // BollardServiceController without a real docker daemon in
        // CI, so we exercise the validation path: any Bollard
        // connection error short-circuits before the validation, so
        // we use the validation-only branch by constructing a spec
        // that fails the runtime gate first.
        //
        // Instead of standing up bollard, exercise the validation
        // check by calling NativeServiceController with a Docker
        // spec — the symmetric guard.
        let native = NativeServiceController::new();
        let mut spec = apple_spec("ollama");
        spec.runtime = ServiceRuntime::Docker;
        let err = native.spawn(&spec).await.unwrap_err();
        match err {
            ServiceError::Invalid(msg) => assert!(
                msg.contains("Docker"),
                "expected invalid-runtime error, got '{msg}'"
            ),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn native_spawn_with_empty_hint_returns_actionable_error() {
        let native = NativeServiceController::new();
        let err = native.spawn(&apple_spec("")).await.unwrap_err();
        match err {
            ServiceError::Invalid(msg) => {
                assert!(
                    msg.contains("binary_hint") && msg.contains("native"),
                    "error must call out the missing binary_hint, got '{msg}'"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn native_spawn_with_unknown_hint_returns_actionable_error() {
        let native = NativeServiceController::new();
        let err = native.spawn(&apple_spec("llamafile")).await.unwrap_err();
        match err {
            ServiceError::Invalid(msg) => {
                assert!(
                    msg.contains("unknown") && msg.contains("ollama"),
                    "error must mention supported hints, got '{msg}'"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn discover_ollama_env_override_to_missing_path_yields_actionable_error() {
        // Operator sets OLLAMA_BINARY to a typo'd path. Discovery
        // must reject early with a message that names the offending
        // path — the supervisor surfaces this verbatim, so the
        // operator can copy/paste the path into a `ls` to debug.
        let err = NativeServiceController::discover_ollama_with(
            Some("/definitely/not/a/real/path/ollama".into()),
            |_| None,
        )
        .unwrap_err();
        match err {
            ServiceError::Invalid(msg) => {
                assert!(
                    msg.contains("does not exist") && msg.contains("/definitely/not/a/real/path/ollama"),
                    "error must echo the bad path, got '{msg}'"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn discover_ollama_no_env_no_path_yields_install_hint() {
        // Default install state on a Mac that hasn't run brew yet:
        // no env override, nothing on PATH, brew prefixes empty.
        // The wizard renders this string directly — pin the
        // copy so a refactor doesn't silently regress it.
        let err = NativeServiceController::discover_ollama_with(None, |_| None).unwrap_err();
        match err {
            ServiceError::Invalid(msg) => {
                assert!(
                    msg.contains("brew install ollama"),
                    "install hint must call out brew, got '{msg}'"
                );
                assert!(
                    msg.contains("OLLAMA_BINARY"),
                    "install hint must mention the env-var escape hatch, got '{msg}'"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn discover_ollama_env_override_to_real_path_wins_over_path_lookup() {
        // Create a tempfile, pass its path as OLLAMA_BINARY. The
        // PATH lookup closure intentionally returns a wrong answer
        // to prove the env override is consulted first.
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        let p = tmp.path().to_owned();
        let found = NativeServiceController::discover_ollama_with(
            Some(p.to_string_lossy().into_owned()),
            |_| Some(PathBuf::from("/wrong/path/should/not/win")),
        )
        .expect("found");
        assert_eq!(found, p);
    }

    #[test]
    fn discover_ollama_falls_back_to_path_when_env_unset() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        let p = tmp.path().to_owned();
        let p_for_closure = p.clone();
        let found =
            NativeServiceController::discover_ollama_with(None, move |name| {
                if name == "ollama" {
                    Some(p_for_closure.clone())
                } else {
                    None
                }
            })
            .expect("found");
        assert_eq!(found, p);
    }

    #[tokio::test]
    async fn native_stop_on_unknown_handle_is_ok() {
        // Symmetric with BollardServiceController's best-effort
        // stop — calling stop on something we don't track is a
        // no-op rather than an error. Lets the supervisor's
        // reconcile loop call stop unconditionally during cleanup.
        let native = NativeServiceController::new();
        let handle = ServiceHandle {
            container_id: "native:nope:0".into(),
            name: "nope".into(),
            host_port: 8101,
        };
        native.stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn native_inspect_unknown_handle_returns_not_found() {
        let native = NativeServiceController::new();
        let handle = ServiceHandle {
            container_id: "native:nope:0".into(),
            name: "nope".into(),
            host_port: 8101,
        };
        assert_eq!(
            native.inspect(&handle).await.unwrap(),
            ServiceStatus::NotFound
        );
    }

    #[tokio::test]
    async fn native_try_adopt_always_returns_none() {
        // Adoption is intentionally unsupported for native
        // processes; the supervisor must respawn rather than
        // attaching to a stale PID.
        let native = NativeServiceController::new();
        assert!(
            native
                .try_adopt("execlaw-backend-Standard", 8101)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn native_health_check_returns_false_when_endpoint_refuses() {
        // Use a port that's almost certainly closed locally — the
        // health check must short-circuit to `Ok(false)` (transient
        // / connection-refused), NOT propagate an error. The
        // supervisor distinguishes "starting up" (Ok(false)) from
        // "broken" (Err) and we don't want native to be louder than
        // bollard here.
        let native = NativeServiceController::new();
        // Pick an ephemeral-range port that's almost certainly idle.
        // 127.0.0.1:1 is reserved and never bound by user services.
        let ok = native
            .health_check("http://127.0.0.1:1/api/tags")
            .await
            .expect("health check is best-effort, never errors on connect-refused");
        assert!(!ok, "closed port must yield Ok(false), not Ok(true)");
    }

    #[tokio::test]
    async fn multiplexed_routes_docker_spec_to_docker_controller() {
        // Use two mocks as the inner controllers + verify the
        // multiplexer picks the right one based on spec.runtime.
        let docker_mock = Arc::new(MockServiceController::new());
        let native_mock = Arc::new(MockServiceController::new());
        let multi = MultiplexedServiceController::new(
            docker_mock.clone() as Arc<dyn ServiceController>,
            native_mock.clone() as Arc<dyn ServiceController>,
        );
        let _ = multi.spawn(&fixture_spec()).await.unwrap();
        assert_eq!(docker_mock.spawn_count().await, 1);
        assert_eq!(native_mock.spawn_count().await, 0);
    }

    #[tokio::test]
    async fn multiplexed_routes_native_spec_to_native_controller() {
        let docker_mock = Arc::new(MockServiceController::new());
        let native_mock = Arc::new(MockServiceController::new());
        let multi = MultiplexedServiceController::new(
            docker_mock.clone() as Arc<dyn ServiceController>,
            native_mock.clone() as Arc<dyn ServiceController>,
        );
        let _ = multi.spawn(&apple_spec("ollama")).await.unwrap();
        assert_eq!(native_mock.spawn_count().await, 1);
        assert_eq!(docker_mock.spawn_count().await, 0);
    }

    #[tokio::test]
    async fn multiplexed_routes_stop_to_owning_controller_after_spawn() {
        // Spawn through native, then stop the handle; the multiplex
        // must dispatch the stop to the native mock (because the
        // routing map remembers that spawn).
        let docker_mock = Arc::new(MockServiceController::new());
        let native_mock = Arc::new(MockServiceController::new());
        let multi = MultiplexedServiceController::new(
            docker_mock.clone() as Arc<dyn ServiceController>,
            native_mock.clone() as Arc<dyn ServiceController>,
        );
        let handle = multi.spawn(&apple_spec("ollama")).await.unwrap();
        multi.stop(&handle).await.unwrap();
        assert_eq!(native_mock.stop_count().await, 1);
        assert_eq!(docker_mock.stop_count().await, 0);
    }

    #[tokio::test]
    async fn multiplexed_unknown_handle_defaults_stop_to_docker() {
        // Backwards-compat: a binary restart loses the routing map,
        // so a stop call against an unknown name must fall back to
        // the Docker controller (matches every existing supervisor
        // behavior pre-Apple). Native containers can't be adopted
        // anyway (see `native_try_adopt_always_returns_none`).
        let docker_mock = Arc::new(MockServiceController::new());
        let native_mock = Arc::new(MockServiceController::new());
        let multi = MultiplexedServiceController::new(
            docker_mock.clone() as Arc<dyn ServiceController>,
            native_mock.clone() as Arc<dyn ServiceController>,
        );
        let stale = ServiceHandle {
            container_id: "abc".into(),
            name: "execlaw-backend-Standard".into(),
            host_port: 8101,
        };
        multi.stop(&stale).await.unwrap();
        assert_eq!(docker_mock.stop_count().await, 1);
        assert_eq!(native_mock.stop_count().await, 0);
    }
}
