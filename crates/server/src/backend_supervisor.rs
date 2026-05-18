//! Backend supervisor (Phase 12.C, MIGRATION_PLAN §5.4 + §5).
//!
//! For every `config_backends` row whose `mode = managed`, the
//! supervisor:
//!
//!   1. Picks a stable host port from a per-purpose pool.
//!   2. Asks the [`ServiceController`] to spawn the inference
//!      service container.
//!   3. Probes the container's HTTP `/health` endpoint until it
//!      reports 2xx → row is marked `Healthy` and `endpoint` is
//!      written back to `config_backends.endpoint`.
//!   4. On crash, exponential-backoff restart up to a hard cap;
//!      after the cap, parks the row in `CrashLooping` until the
//!      operator edits + saves again (which kicks the supervisor).
//!
//! Mode-flip semantics: switching a row from external→managed kicks
//! a spawn; managed→external stops the running container and clears
//! `endpoint`. The supervisor reconciles desired vs running state on
//! every tick + on every `kick()`.
//!
//! Production wires `BollardServiceController`; tests wire
//! `MockServiceController` (gated behind container-manager's
//! `test-mock` feature).

use execlaw_container_manager::{
    DownloadEvent, GpuVendor, HfDownloader, HostMount, ServiceController, ServiceError,
    ServiceHandle, ServiceSpec, ServiceStatus,
};
use execlaw_core::Database;
use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use execlaw_core::backends::{BackendMode, BackendPurpose, BackendRow, BackendStore};
use execlaw_core::ids::AlertId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

/// Default sweep cadence — every 5 seconds the supervisor reconciles
/// desired vs running state. Operators get visible-state-change
/// latency under that.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Hard cap on consecutive restart attempts before the supervisor
/// parks the row in `CrashLooping`. Mirrors selfhosted-claw's
/// "5-attempt then cooldown" pattern in `docs/runner-design.md`.
pub const MAX_RESTART_ATTEMPTS: u32 = 5;

/// Per-purpose stable host port. The supervisor binds these so the
/// runner's URL (`http://127.0.0.1:{port}/v1`) doesn't churn across
/// restarts. Picked to avoid common conflicts (vLLM defaults to
/// 8000; we use 8101+).
pub fn host_port_for(purpose: BackendPurpose) -> u16 {
    match purpose {
        BackendPurpose::Standard => 8101,
        BackendPurpose::Small => 8102,
        BackendPurpose::VoiceStt => 8103,
        BackendPurpose::VoiceTts => 8104,
    }
}

/// Higher-resolution lifecycle stage the supervisor reports
/// alongside `ServiceStatus`. The mapping is many-to-one — a single
/// `ServiceStatus::Starting` covers everything from "we just kicked
/// Docker create" through "vLLM is busy decompressing 27 GB of
/// AWQ weights into VRAM" — so the SPA reads `stage` to render the
/// right pill copy + helpful message at each phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleStage {
    /// No container yet (default state for a fresh slot).
    Idle,
    /// Host-side `HfDownloader` is downloading model weights into
    /// `~/.execlaw/hf-cache/`. The supervisor blocks the container
    /// spawn until this completes — the operator sees real progress
    /// (`Downloading model · 8.5/18 GB`) instead of "Provisioning"
    /// for the 5–15 minutes the download takes.
    DownloadingModel,
    /// Bollard is pulling layers from the registry.
    PullingImage,
    /// Container is starting up but the in-container service hasn't
    /// initialised yet (TCP listener not bound, model not loaded).
    ContainerStarting,
    /// Container is up + the service is initialising. For LLM
    /// containers this is the model-load phase; can take 5+
    /// minutes for 27B AWQ.
    LoadingModel,
    /// `/health` succeeded recently — the service is accepting
    /// requests.
    Healthy,
    /// Container exited / kept exiting / never started.
    Failed,
}

impl LifecycleStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleStage::Idle => "Idle",
            LifecycleStage::DownloadingModel => "DownloadingModel",
            LifecycleStage::PullingImage => "PullingImage",
            LifecycleStage::ContainerStarting => "ContainerStarting",
            LifecycleStage::LoadingModel => "LoadingModel",
            LifecycleStage::Healthy => "Healthy",
            LifecycleStage::Failed => "Failed",
        }
    }
}

/// Bytes-progress snapshot for an in-flight HF model download.
/// Surfaced in `BackendRuntimeStatus.download_progress` so the SPA
/// can render `Downloading · 8.5 / 18.0 GB · 47%` in the pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub file_idx: usize,
    pub file_count: usize,
}

/// Heuristic: pick the lifecycle stage from the latest log line.
/// vLLM prints distinct status messages for each phase; matching on
/// substrings lets us flip from `ContainerStarting` →
/// `LoadingModel` → `Healthy` without needing the inference plugin
/// to surface structured signals. Conservative defaults keep the
/// stage at `ContainerStarting` until we see a clear "loading"
/// marker — better than mis-classifying.
pub(crate) fn classify_stage_from_log(line: Option<&str>) -> Option<LifecycleStage> {
    let l = line?.to_ascii_lowercase();
    // vLLM: "Starting to load model …", "Loading safetensors
    // checkpoint shards", "Capturing CUDA graph shapes"
    // Whisper / Kokoro: similar "loading model" lines.
    if l.contains("starting to load model")
        || l.contains("loading safetensors")
        || l.contains("loading checkpoint")
        || l.contains("loading model")
        || l.contains("capturing cuda graph")
        || l.contains("loading weights")
        || l.contains("model weights")
    {
        return Some(LifecycleStage::LoadingModel);
    }
    None
}

/// Live status the SPA reads via `/api/admin/backends/{purpose}/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRuntimeStatus {
    pub purpose: BackendPurpose,
    pub mode: BackendMode,
    pub status: ServiceStatus,
    pub endpoint: Option<String>,
    /// Restart attempts since last health. Only populated for
    /// CrashLooping; 0 otherwise.
    pub restart_attempts: u32,
    /// Higher-resolution lifecycle phase. See `LifecycleStage`.
    pub stage: LifecycleStage,
    /// Wall-clock seconds since the most recent successful spawn.
    /// `None` when no spawn has happened yet (Idle / external rows).
    pub elapsed_secs: Option<u64>,
    /// Last meaningful log line from the running container —
    /// surfaced in the SPA pill so an operator can see what the
    /// service is currently doing without opening the logs modal.
    pub last_log_line: Option<String>,
    /// In-flight HF model download progress. Populated only while
    /// stage = `DownloadingModel`; cleared once the download
    /// completes and the supervisor moves on to spawn.
    pub download_progress: Option<DownloadProgress>,
}

#[derive(Debug, Clone)]
struct ManagedSlot {
    /// Set after a successful `spawn`; cleared on stop.
    handle: Option<ServiceHandle>,
    /// Last status the supervisor observed.
    status: ServiceStatus,
    /// Increments on each consecutive restart attempt; resets to 0
    /// on a Healthy observation.
    restart_attempts: u32,
    /// Most recent container log tail. Captured every reconcile
    /// pass when a handle is present, so the operator can see WHY
    /// a container died after the supervisor reaped it AND can see
    /// "what is the in-container service doing right now" while it
    /// warms up. `/api/admin/backends/{purpose}/logs` reads this.
    last_log_tail: Option<String>,
    /// Single most-recent non-blank log line, used to render a
    /// one-line progress signal in the SPA pill (e.g.
    /// "Loading safetensors checkpoint shards (3/12)"). Updated in
    /// the same reconcile pass that updates `last_log_tail`.
    last_log_line: Option<String>,
    /// Wall-clock unix time at the most recent successful spawn.
    /// `None` when no spawn has happened yet. Drives the SPA's
    /// "Loading model · 4m elapsed" copy so an operator can
    /// distinguish "still warming up normally" from "stuck for an
    /// hour, something's wrong".
    spawn_started_at: Option<i64>,
    /// Higher-resolution lifecycle stage. `ServiceStatus` collapses
    /// "container starting" and "service warming up" into a single
    /// `Starting` variant, which left the SPA's "Provisioning" pill
    /// stuck for 5+ minutes during long Qwen-3.5-27B model loads
    /// with no signal that progress was happening. `stage` lets the
    /// SPA render the right copy at each phase.
    stage: LifecycleStage,
    /// Snapshot of the in-flight HF model download. `Some` only
    /// while `stage == DownloadingModel`; cleared at spawn time
    /// (or on Idle / Healthy / Failed). Updated by the download
    /// progress task that the supervisor spawns when reconciling
    /// a row whose model isn't yet in the host's primary cache.
    download_progress: Option<DownloadProgress>,
    /// JoinHandle of the in-flight download task. Held so we can
    /// detect "download finished" between reconcile ticks; cleared
    /// once we reap it. None when no download is currently
    /// running.
    download_task: Option<DownloadTaskState>,
}

/// Per-slot state for an in-flight host-side HF download. The
/// supervisor's reconcile pass polls `done` to decide whether the
/// download has finished and the container can be spawned.
#[derive(Debug, Clone)]
struct DownloadTaskState {
    /// Kept for diagnostics + future restart-on-model-change logic.
    #[allow(dead_code)]
    model_id: String,
    /// Set to `true` by the download task when it terminates
    /// (success or failure). The supervisor reads this on each
    /// reconcile to decide whether to advance the stage.
    done: Arc<std::sync::atomic::AtomicBool>,
    /// Set to `Some(error)` on failure; remains `None` on success.
    /// Mutated by the download task; read by the reconcile.
    failure: Arc<Mutex<Option<String>>>,
    /// Most recent progress snapshot. Lock-light because it's a
    /// single struct copy; Mutex (rather than ArcSwap) because we
    /// already pull tokio in.
    progress: Arc<Mutex<Option<DownloadProgress>>>,
}

impl Default for ManagedSlot {
    fn default() -> Self {
        Self {
            handle: None,
            // Slots default to Stopped — the supervisor's first
            // pass observes this and decides spawn vs no-op based on
            // the row's mode.
            status: ServiceStatus::Stopped,
            restart_attempts: 0,
            last_log_tail: None,
            last_log_line: None,
            spawn_started_at: None,
            stage: LifecycleStage::Idle,
            download_progress: None,
            download_task: None,
        }
    }
}

/// Pulls the operator's spawn config out of the row's
/// `model_spec_json`. Schema convention:
///
/// ```jsonc
/// {
///   "image": "vllm/vllm-openai:v0.6.2",
///   "args": ["--model", "Qwen3.5-27B-AWQ"],
///   "env": [["HF_HOME", "/cache"]],
///   "container_port": 8000
/// }
/// ```
///
/// Missing fields fall back to sensible defaults; an unparseable
/// blob produces a placeholder spec that the supervisor refuses to
/// spawn (status stays `Stopped` until the operator fixes it).
fn spec_from_row(row: &BackendRow) -> Result<ServiceSpec, String> {
    let obj = row
        .model_spec_json
        .as_object()
        .ok_or_else(|| "model_spec_json must be a JSON object for managed mode".to_string())?;
    // Apple-Silicon native presets (Ollama) omit `image` because they
    // spawn a host-native subprocess, not a Docker container. We
    // accept the missing field for `runtime: "native"` rows and pass
    // an empty image through; the validation in `NativeServiceController`
    // doesn't consult `image` at all.
    let is_native_runtime = obj
        .get("runtime")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("native"))
        .unwrap_or(false);
    let image = match obj.get("image").and_then(|v| v.as_str()) {
        Some(s) => s,
        None if is_native_runtime => "",
        None => return Err("managed model_spec_json must include `image`".to_string()),
    };
    let mut args = obj
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    inject_required_vllm_tool_args(image, &args.clone(), &mut args);
    let env = obj
        .get("env")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|pair| {
                    let arr = pair.as_array()?;
                    let k = arr.first()?.as_str()?.to_owned();
                    let v = arr.get(1)?.as_str()?.to_owned();
                    Some((k, v))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let container_port = obj
        .get("container_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(8000) as u16;
    // The wizard writes a `gpu_vendor` discriminator into model_spec
    // so the controller knows which device-passthrough strategy to
    // use without re-probing. Older rows that pre-date the field
    // fall through to `None` (no passthrough) — safer than guessing
    // wrong and bricking create_container.
    let gpu_vendor = obj
        .get("gpu_vendor")
        .and_then(|v| v.as_str())
        .and_then(parse_gpu_vendor);
    // `model_spec.mounts` is an array of
    //   { "host_path": "...", "container_path": "...", "read_only": bool }
    // objects. We default to an empty list when absent so older rows
    // pre-Phase-14.C keep behaving exactly as before. The supervisor
    // adds the host's primary HF cache mount at runtime if no entry
    // for `/root/.cache/huggingface` is present.
    let mounts: Vec<HostMount> = obj
        .get("mounts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let host = m.get("host_path")?.as_str()?.to_owned();
                    let container = m.get("container_path")?.as_str()?.to_owned();
                    let read_only = m.get("read_only").and_then(|v| v.as_bool()).unwrap_or(true);
                    Some(HostMount {
                        host_path: host,
                        container_path: container,
                        read_only,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // Dispatch on the optional `runtime` discriminator in
    // model_spec_json. Apple-Silicon Ollama presets set
    // `"runtime": "native"` + `"binary_hint": "ollama"` so the
    // multiplexed controller routes the spawn to
    // `NativeServiceController` instead of bollard. Other rows omit
    // the field and default to Docker (matches every existing
    // managed-mode preset).
    let (runtime, binary_hint) = match row.model_spec_json.get("runtime").and_then(|v| v.as_str()) {
        Some("native") => {
            let hint = row
                .model_spec_json
                .get("binary_hint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            (execlaw_container_manager::ServiceRuntime::Native, hint)
        }
        _ => (
            execlaw_container_manager::ServiceRuntime::Docker,
            String::new(),
        ),
    };
    Ok(ServiceSpec {
        name: format!("execlaw-backend-{}", row.purpose.as_str()),
        image: image.to_owned(),
        args,
        entrypoint: None,
        env,
        gpu_id: row.gpu_id.clone(),
        gpu_vendor,
        mounts,
        host_port: host_port_for(row.purpose),
        container_port,
        runtime,
        binary_hint,
    })
}

/// vLLM rejects any request that carries a `tools` array unless the
/// server was launched with `--enable-auto-tool-choice` and a
/// `--tool-call-parser=<parser>` matching the model's tool-call
/// format (400: `"auto" tool choice requires --enable-auto-tool-choice
/// and --tool-call-parser to be set`). The runner ALWAYS sends a tool
/// catalogue (built-in tools register at boot), so without these
/// flags every chat fires "opening inference stream" and 500s.
///
/// We inject the missing flags here rather than in the preset library
/// so existing operator rows (`config_backends.model_spec_json`)
/// recover automatically — the supervisor recreates the container on
/// the next reconcile pass and picks up the new args.
///
/// Operator overrides win: if the existing args already specify
/// `--enable-auto-tool-choice` or `--tool-call-parser`, we don't
/// touch them. The parser default is `hermes` (Qwen3 + Qwen2.5 ship
/// with a Hermes-shaped tool grammar). Non-Qwen models that need a
/// different parser should set it explicitly in the wizard.
fn inject_required_vllm_tool_args(image: &str, original: &[String], out: &mut Vec<String>) {
    if !is_vllm_image(image) {
        return;
    }
    let has_enable = original
        .iter()
        .any(|a| a == "--enable-auto-tool-choice" || a.starts_with("--enable-auto-tool-choice="));
    if !has_enable {
        out.push("--enable-auto-tool-choice".into());
    }
    let has_parser = original
        .iter()
        .any(|a| a == "--tool-call-parser" || a.starts_with("--tool-call-parser="));
    if !has_parser {
        let parser = default_tool_parser_for_args(original);
        out.push(format!("--tool-call-parser={parser}"));
    }
    // 2026-05-02 — prefix caching is OFF by default in vLLM v1, but
    // execlaw's chat path benefits enormously: the system prompt +
    // tool catalogue (~7KB / ~2K tokens) is byte-for-byte identical
    // across every round of a multi-step turn, so caching the
    // common prefix saves the prompt-prefill cost on rounds 2+.
    // Per the engine logs (Avg prompt throughput: 1627 tok/s vs
    // generation 8.5 tok/s), prefill is the dominant cost — a
    // 3-round web_search → web_fetch → synthesise turn drops from
    // ~25s to ~10s with this enabled.
    let has_prefix = original
        .iter()
        .any(|a| a == "--enable-prefix-caching" || a == "--no-enable-prefix-caching");
    if !has_prefix {
        out.push("--enable-prefix-caching".into());
    }
}

fn is_vllm_image(image: &str) -> bool {
    let lower = image.to_ascii_lowercase();
    lower.contains("vllm")
}

/// Pick a tool-call parser based on the model id present in `args`.
///
/// 2026-05-02 — flipped Qwen's default from `hermes` to
/// `qwen3_xml`. Empirically Qwen3.5-AWQ emits its tool calls in the
/// native Qwen3 XML form:
///
/// ```text
/// <tool_call>
/// <function=web_search>
///   <parameter=query>Paris weather forecast today</parameter>
/// </function>
/// </tool_call>
/// ```
///
/// vLLM's hermes parser, which expects `<tool_call>{json}</tool_call>`,
/// sets `finish_reason=tool_calls` on the response (the closing tag
/// triggers it) but extracts zero structured tool_calls — and the
/// runner then commits "(empty response)" because its `tool_calls`
/// vec is empty. The `qwen3_xml` parser ships with vLLM 0.20+ and
/// handles the native format directly.
fn default_tool_parser_for_args(args: &[String]) -> &'static str {
    let model = args
        .iter()
        .find_map(|a| a.strip_prefix("--model="))
        .or_else(|| {
            // Tolerate the `["--model", "X"]` shape too.
            let mut iter = args.iter();
            while let Some(a) = iter.next() {
                if a == "--model" {
                    return iter.next().map(|s| s.as_str());
                }
            }
            None
        })
        .unwrap_or("");
    let lower = model.to_ascii_lowercase();
    if lower.contains("llama-3") || lower.contains("llama3") {
        "llama3_json"
    } else if lower.contains("mistral") {
        "mistral"
    } else if lower.contains("qwen3.6") || lower.contains("qwen3_6") {
        // Qwen3.6 uses the `qwen3_coder` parser per vLLM Recipes
        // (https://recipes.vllm.ai/Qwen/Qwen3.6-27B). Distinct from
        // Qwen3 / Qwen3.5 which emit native XML — Qwen3.6's
        // tool-call output shape is different enough that
        // `qwen3_xml` silently extracts zero tool_calls.
        "qwen3_coder"
    } else if lower.contains("qwen3") {
        // Qwen3 + Qwen3.5 emit XML-shaped tool calls; hermes
        // silently fails on them.
        "qwen3_xml"
    } else if lower.contains("qwen") {
        // Qwen 2.5 and earlier are Hermes-shaped.
        "hermes"
    } else {
        // Conservative default for any unknown model.
        "hermes"
    }
}

/// HTTP path the supervisor probes for liveness. Engine-specific
/// because every inference daemon picks a different convention:
///
///   * **Ollama** (`binary_hint = "ollama"`) — `/api/tags`. Returns
///     200 + a JSON list of locally-cached models. Ollama doesn't
///     ship a `/health` endpoint.
///   * **vLLM / Whisper / Kokoro / Piper** (Docker runtime, or
///     `binary_hint == ""`) — `/health`. Convention shared by every
///     execlaw-built service image.
///
/// Returns a leading `/` so the supervisor can string-concat onto
/// `ServiceHandle::endpoint_url("http")` without thinking about it.
///
/// **Known gap (Apple Silicon Ollama):** the Ollama daemon answers
/// `/api/tags` ~instantly on startup, well before any model is
/// pulled. So a fresh wizard run will mark the backend `Healthy` and
/// write the `/v1` endpoint — but the first `/v1/chat/completions`
/// request will 404 with "model 'X' not found" because Ollama
/// doesn't auto-pull on the OpenAI-compat path. v1 punts on the
/// active-pull integration; operators must run
/// `ollama pull <model>` manually after configuring the wizard. The
/// follow-up (tracked in the plan's audit gaps) wires
/// `POST /api/pull` into the supervisor's existing download-task
/// machinery so the SPA can surface "Pulling 47%…" the same way it
/// does for HF model downloads on vLLM today.
fn health_path_for(spec: &ServiceSpec) -> &'static str {
    health_path_for_hint(&spec.binary_hint)
}

/// String-only form used by the reconcile loop's health-check arm
/// (the loop doesn't keep a `ServiceSpec` in scope after spawn —
/// only the `BackendRow` and `ServiceHandle`).
fn health_path_for_hint(hint: &str) -> &'static str {
    match hint {
        "ollama" => "/api/tags",
        _ => "/health",
    }
}

/// Extract the binary_hint from a `BackendRow`'s `model_spec_json`
/// without re-running `spec_from_row`. Returns an empty string when
/// the row pre-dates the native-runtime work, which yields `/health`
/// from `health_path_for_hint` — exactly the legacy behaviour.
fn binary_hint_from_row(row: &BackendRow) -> &str {
    row.model_spec_json
        .get("binary_hint")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn parse_gpu_vendor(s: &str) -> Option<GpuVendor> {
    match s.trim().to_ascii_lowercase().as_str() {
        "nvidia" => Some(GpuVendor::Nvidia),
        "intel" => Some(GpuVendor::Intel),
        "amd" => Some(GpuVendor::Amd),
        "apple" => Some(GpuVendor::Apple),
        _ => None,
    }
}

/// Long-running supervisor task.
#[derive(Clone)]
pub struct BackendSupervisor {
    db: Database,
    controller: Arc<dyn ServiceController>,
    /// Optional host-side HF downloader. When wired the supervisor
    /// blocks every managed-row spawn behind a cache check + (if
    /// missing) a download. Tests don't pass one, so the cache step
    /// is skipped and the supervisor falls back to letting the
    /// container do its own download (legacy behaviour).
    hf: Option<HfDownloader>,
    /// Path the supervisor mounts read-only into every managed
    /// container as `/root/.cache/huggingface` so vLLM finds the
    /// pre-downloaded weights. `None` means no mount is added —
    /// the container falls back to its own download path.
    hf_primary_cache: Option<std::path::PathBuf>,
    interval: Duration,
    kick: Arc<Notify>,
    /// Per-purpose runtime state, keyed by purpose-string.
    slots: Arc<Mutex<HashMap<String, ManagedSlot>>>,
}

impl BackendSupervisor {
    pub fn new(db: Database, controller: Arc<dyn ServiceController>) -> Self {
        Self::with_config(db, controller, DEFAULT_TICK_INTERVAL)
    }

    pub fn with_config(
        db: Database,
        controller: Arc<dyn ServiceController>,
        interval: Duration,
    ) -> Self {
        Self {
            db,
            controller,
            hf: None,
            hf_primary_cache: None,
            interval,
            kick: Arc::new(Notify::new()),
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Wire the host-side HF downloader. Call this once at boot
    /// from the CLI's startup task; nothing else does.
    pub fn with_hf_downloader(
        mut self,
        downloader: HfDownloader,
        primary_cache: std::path::PathBuf,
    ) -> Self {
        self.hf = Some(downloader);
        self.hf_primary_cache = Some(primary_cache);
        self
    }

    /// Force a reconcile pass. Operators trigger this from a
    /// "restart" button or after editing a Backend row, so the
    /// supervisor doesn't have to wait the full tick interval.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Reset the per-purpose restart counter. Called by the upsert
    /// handler after every save so a row that was parked at
    /// `MAX_RESTART_ATTEMPTS` gets a fresh runway when the operator
    /// edits it (presumed-fixed config). Does not touch the
    /// `handle` — the next reconcile pass decides spawn vs no-op
    /// based on the row's current spec.
    pub async fn reset_attempts(&self, purpose: BackendPurpose) {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(purpose.as_str()) {
            slot.restart_attempts = 0;
            // If the slot was parked CrashLooping, drop the status
            // so the next reconcile makes a fresh determination
            // instead of re-reading "looping past cap".
            if matches!(slot.status, ServiceStatus::CrashLooping { .. }) {
                slot.status = ServiceStatus::Stopped;
                slot.handle = None;
            }
        }
    }

    /// Read-only status snapshot for the SPA. Returns one entry per
    /// configured backend (any mode); `external` rows always report
    /// `Stopped` here because the supervisor doesn't manage them.
    pub async fn snapshot_status(&self) -> Vec<BackendRuntimeStatus> {
        let store = BackendStore::new(&self.db);
        let rows = match store.list_all() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let slots = self.slots.lock().await;
        let now = chrono::Utc::now().timestamp();
        rows.into_iter()
            .map(|row| {
                let slot = slots.get(row.purpose.as_str());
                let status = slot
                    .map(|s| s.status.clone())
                    .unwrap_or(ServiceStatus::Stopped);
                let restart_attempts = slot.map(|s| s.restart_attempts).unwrap_or(0);
                let stage = slot.map(|s| s.stage).unwrap_or(LifecycleStage::Idle);
                let elapsed_secs = slot
                    .and_then(|s| s.spawn_started_at)
                    .map(|t| now.saturating_sub(t).max(0) as u64);
                let last_log_line = slot.and_then(|s| s.last_log_line.clone());
                let download_progress = slot.and_then(|s| s.download_progress);
                BackendRuntimeStatus {
                    purpose: row.purpose,
                    mode: row.mode,
                    status,
                    endpoint: row.endpoint,
                    restart_attempts,
                    stage,
                    elapsed_secs,
                    last_log_line,
                    download_progress,
                }
            })
            .collect()
    }

    /// Drive the loop until `stop` is notified.
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "backend supervisor running"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("backend supervisor stop received; exiting");
                    return;
                }
            }
            self.reconcile_once().await;
        }
    }

    /// One reconcile pass. Public so tests can drive it
    /// deterministically.
    pub async fn reconcile_once(&self) {
        let store = BackendStore::new(&self.db);
        let rows = match store.list_all() {
            Ok(r) => r,
            Err(e) => {
                warn!("backend supervisor: failed to list backends: {e}");
                return;
            }
        };
        let mut slots = self.slots.lock().await;

        // Stop slots whose corresponding row is gone or has flipped
        // away from managed.
        let configured: HashMap<String, BackendRow> = rows
            .iter()
            .map(|r| (r.purpose.as_str().to_owned(), r.clone()))
            .collect();
        let to_drop: Vec<String> = slots
            .iter()
            .filter_map(|(p, slot)| {
                let row = configured.get(p);
                let still_managed = matches!(row.map(|r| r.mode), Some(BackendMode::Managed));
                if !still_managed && slot.handle.is_some() {
                    Some(p.clone())
                } else {
                    None
                }
            })
            .collect();
        for p in to_drop {
            if let Some(slot) = slots.get_mut(&p) {
                if let Some(handle) = slot.handle.take() {
                    debug!(
                        purpose = %p,
                        "supervisor stopping container (mode flipped or row removed)"
                    );
                    let _ = self.controller.stop(&handle).await;
                    // Don't clear `endpoint` here — when mode flips
                    // to external, the operator's same upsert
                    // typically set a new external URL we mustn't
                    // clobber. Stale loopback URLs from our prior
                    // managed run get overwritten the moment the
                    // row's next save provides a real endpoint;
                    // until then the runner sees a (stale) URL
                    // it'll fail to reach, which surfaces as an
                    // alert rather than a silent misroute.
                }
                slot.status = ServiceStatus::Stopped;
                slot.restart_attempts = 0;
            }
        }

        // Reconcile every managed row.
        for row in rows.iter().filter(|r| r.mode == BackendMode::Managed) {
            let key = row.purpose.as_str().to_owned();
            let slot = slots.entry(key.clone()).or_default();

            // If we're already CrashLooping past the cap, do nothing
            // until the operator's next save kicks the supervisor.
            if matches!(slot.status, ServiceStatus::CrashLooping { .. })
                && slot.restart_attempts >= MAX_RESTART_ATTEMPTS
            {
                continue;
            }

            // Need a handle? First make sure the model is cached
            // host-side (Phase 14.C). The supervisor blocks the
            // spawn until the downloader reports complete; while
            // the download is in flight we surface progress in
            // `slot.download_progress` so the SPA pill shows
            // "Downloading model · 8.5 / 18.0 GB · 47%".
            //
            // Phase 14.E — before spawning fresh, try to adopt an
            // existing container with the expected name. Cargo-watch
            // restarts the binary every time a `.rs` file changes;
            // without adoption, every restart force-removes the
            // running vLLM container (which is in the middle of a
            // 3–5 minute model load) and spawns a brand-new one
            // that has to reload the same 27 GB of weights from
            // scratch. From the operator's POV, this looks like a
            // 5-minute restart loop. Adopting matches the operator's
            // intuition: "the container is already running; leave
            // it alone."
            if slot.handle.is_none() {
                let expected_name = format!("execlaw-backend-{}", row.purpose.as_str());
                match self
                    .controller
                    .try_adopt(&expected_name, host_port_for(row.purpose))
                    .await
                {
                    Ok(Some(handle)) => {
                        info!(
                            purpose = %key,
                            container = %handle.container_id,
                            "adopted existing managed container (binary restart)"
                        );
                        slot.handle = Some(handle);
                        slot.status = ServiceStatus::Starting;
                        slot.stage = LifecycleStage::ContainerStarting;
                        // Best-effort: we don't know how long the
                        // adopted container has been up. Set the
                        // start time to "now" so the SPA's elapsed
                        // counter starts from this binary's lifetime
                        // — operators treat that as "time since I
                        // started watching", which is the most
                        // useful framing.
                        slot.spawn_started_at = Some(chrono::Utc::now().timestamp());
                        slot.restart_attempts = 0;
                        // Fall through to the inspect+probe block
                        // below; if /health succeeds on the next
                        // tick, status flips to Healthy as usual.
                    }
                    Ok(None) => {
                        // No running container with that name; fall
                        // through to the existing spawn path.
                    }
                    Err(e) => {
                        // Don't bail on a transient inspect error —
                        // log + fall through to spawn. spawn() will
                        // also clean up any stale container.
                        warn!(
                            purpose = %key,
                            "try_adopt failed: {e}; will spawn fresh"
                        );
                    }
                }
            }

            if slot.handle.is_none() {
                let mut spec = match spec_from_row(row) {
                    Ok(s) => s,
                    Err(msg) => {
                        warn!(
                            purpose = %key,
                            "managed backend has invalid model_spec_json: {msg}"
                        );
                        slot.status = ServiceStatus::Stopped;
                        slot.stage = LifecycleStage::Failed;
                        emit_crashloop_alert(
                            &self.db,
                            row.purpose,
                            &spec_image_for_alert(row),
                            &format!("invalid model_spec_json: {msg}"),
                        );
                        continue;
                    }
                };

                // Pre-spawn: ensure model is in the host cache.
                // This branch is only taken when an HfDownloader is
                // wired (production); tests pass `None` and skip.
                if let Some(downloader) = self.hf.as_ref() {
                    if let Some(model_id) = extract_model_id(row) {
                        // Check if there's already a download
                        // running for this slot. If yes, poll its
                        // state.
                        if let Some(task) = slot.download_task.clone() {
                            // Mirror progress into the slot's
                            // public field so snapshot_status sees it.
                            if let Ok(p) = task.progress.try_lock() {
                                slot.download_progress = *p;
                            }
                            if !task.done.load(std::sync::atomic::Ordering::SeqCst) {
                                // Still downloading — leave the
                                // slot in DownloadingModel and skip
                                // the spawn arm this tick.
                                slot.stage = LifecycleStage::DownloadingModel;
                                slot.status = ServiceStatus::Pulling;
                                continue;
                            }
                            // Done — inspect failure. Drop the task
                            // either way before falling through.
                            let failure = task.failure.lock().await.clone();
                            slot.download_task = None;
                            slot.download_progress = None;
                            if let Some(err) = failure {
                                warn!(
                                    purpose = %key,
                                    model = %model_id,
                                    "HF model download failed: {err}"
                                );
                                slot.status = ServiceStatus::CrashLooping {
                                    restart_count: slot.restart_attempts + 1,
                                };
                                slot.stage = LifecycleStage::Failed;
                                slot.restart_attempts += 1;
                                emit_crashloop_alert(
                                    &self.db,
                                    row.purpose,
                                    &spec.image,
                                    &format!(
                                        "host-side HuggingFace download for {model_id} failed: {err}"
                                    ),
                                );
                                continue;
                            }
                            info!(
                                purpose = %key,
                                model = %model_id,
                                "HF model download complete; proceeding to spawn"
                            );
                        } else if !downloader.is_cached(&model_id).await {
                            // Not cached + no task in flight → kick
                            // a download task and bail this tick.
                            // Next reconcile picks up progress.
                            info!(
                                purpose = %key,
                                model = %model_id,
                                "model not in host cache; starting download"
                            );
                            let task = spawn_download_task(downloader.clone(), &model_id);
                            slot.download_task = Some(task);
                            slot.stage = LifecycleStage::DownloadingModel;
                            slot.status = ServiceStatus::Pulling;
                            continue;
                        }

                        // Cached (or just finished) — make sure
                        // the container's spec carries the cache
                        // mount + the env var so HF libraries
                        // resolve it without a network call.
                        if let Some(cache) = self.hf_primary_cache.as_ref() {
                            attach_hf_cache_mount(&mut spec, cache);
                        }
                    }
                }

                slot.status = ServiceStatus::Pulling;
                slot.stage = LifecycleStage::PullingImage;
                match self.controller.spawn(&spec).await {
                    Ok(handle) => {
                        info!(
                            purpose = %key,
                            container = %handle.container_id,
                            host_port = handle.host_port,
                            "managed backend spawned"
                        );
                        slot.handle = Some(handle);
                        slot.status = ServiceStatus::Starting;
                        slot.stage = LifecycleStage::ContainerStarting;
                        slot.spawn_started_at = Some(chrono::Utc::now().timestamp());
                        slot.last_log_line = None;
                        slot.last_log_tail = None;
                        slot.download_progress = None;
                        slot.download_task = None;
                        slot.restart_attempts = 0;
                    }
                    Err(e) => {
                        warn!(purpose = %key, "spawn failed: {e}");
                        slot.status = ServiceStatus::CrashLooping {
                            restart_count: slot.restart_attempts + 1,
                        };
                        slot.stage = LifecycleStage::Failed;
                        slot.restart_attempts += 1;
                        emit_crashloop_alert(
                            &self.db,
                            row.purpose,
                            &spec.image,
                            &format!("container spawn failed: {e}"),
                        );
                        continue;
                    }
                }
            }

            // Have a handle: probe it.
            let handle = slot.handle.clone().expect("handle present after spawn arm");
            let inspect = match self.controller.inspect(&handle).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(purpose = %key, "inspect failed: {e}");
                    continue;
                }
            };
            match inspect {
                ServiceStatus::NotFound => {
                    // Container vanished externally; drop the
                    // handle so the next pass respawns.
                    slot.handle = None;
                    slot.status = ServiceStatus::Stopped;
                    slot.stage = LifecycleStage::Idle;
                    let _ = store.set_endpoint(row.purpose, None, chrono::Utc::now().timestamp());
                }
                ServiceStatus::CrashLooping { restart_count } => {
                    slot.status = ServiceStatus::CrashLooping { restart_count };
                    slot.stage = LifecycleStage::Failed;
                    slot.restart_attempts = restart_count.max(slot.restart_attempts + 1);
                    let image_for_alert = row
                        .model_spec_json
                        .get("image")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown image)")
                        .to_owned();
                    // Capture the last ~80 lines of container output
                    // BEFORE we stop+remove the container, so the
                    // alert tells the operator why it died (CUDA
                    // OOM, missing model, HF auth) rather than just
                    // "crash-looping".
                    let log_tail = match self.controller.tail_logs(&handle, 80).await {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(
                                purpose = %key,
                                "failed to tail container logs: {e}"
                            );
                            String::new()
                        }
                    };
                    let detail = if log_tail.trim().is_empty() {
                        format!(
                            "inference container crash-looping (restart_count={restart_count}); container produced no log output"
                        )
                    } else {
                        format!(
                            "inference container crash-looping (restart_count={restart_count}). Last lines of container output:\n\n{}",
                            tail_truncate(&log_tail, 8 * 1024)
                        )
                    };
                    if !log_tail.trim().is_empty() {
                        slot.last_log_line = log_tail
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .map(|l| l.trim().to_owned());
                    }
                    slot.last_log_tail = Some(log_tail);
                    emit_crashloop_alert(&self.db, row.purpose, &image_for_alert, &detail);
                    if slot.restart_attempts < MAX_RESTART_ATTEMPTS {
                        // Stop + drop handle so the next tick respawns.
                        let _ = self.controller.stop(&handle).await;
                        slot.handle = None;
                    }
                    let _ = store.set_endpoint(row.purpose, None, chrono::Utc::now().timestamp());
                }
                ServiceStatus::Stopped => {
                    slot.status = ServiceStatus::Stopped;
                    slot.stage = LifecycleStage::Idle;
                    slot.handle = None;
                }
                _ => {
                    // Starting or Healthy: capture log tail + probe.
                    // We only tail logs while the slot is NOT yet
                    // Healthy — once /health succeeds the SPA stops
                    // showing the warm-up copy, so spending a
                    // docker-logs round-trip per 5s is wasted work.
                    // The moment a future reconcile flips us back
                    // out of Healthy (CrashLooping observation), the
                    // CrashLooping arm captures a fresh tail for
                    // the alert detail.
                    if !matches!(slot.stage, LifecycleStage::Healthy) {
                        let tail = self
                            .controller
                            .tail_logs(&handle, 40)
                            .await
                            .unwrap_or_default();
                        if !tail.trim().is_empty() {
                            slot.last_log_line = tail
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .map(|l| l.trim().to_owned());
                            slot.last_log_tail = Some(tail);
                        }
                    }
                    let url = format!(
                        "{}{}",
                        handle.endpoint_url("http"),
                        health_path_for_hint(binary_hint_from_row(&row))
                    );
                    match self.controller.health_check(&url).await {
                        Ok(true) => {
                            // Ollama-native gate: the daemon's
                            // `/api/tags` answers 200 the instant
                            // `ollama serve` binds its port — long
                            // before any model is in the cache. If
                            // we flipped to Healthy here the first
                            // chat would 404 with "model 'X' not
                            // found" (the v1 punt documented in
                            // docs/setup-mac.md). Instead, before
                            // promoting, check if the configured
                            // model is in the local cache; if not,
                            // POST /api/pull and hold the slot in
                            // DownloadingModel until the pull
                            // finishes. See
                            // crates/server/src/ollama_puller.rs.
                            //
                            // We reuse `slot.download_task` /
                            // `slot.download_progress` — a single
                            // row can't have an HF download AND an
                            // Ollama pull running concurrently
                            // (runtime selects exactly one path), so
                            // sharing the field keeps the state
                            // machine simple.
                            if is_ollama_row(&row) {
                                if let Some(model_id) = extract_ollama_model_id(&row) {
                                    let host_port = handle.host_port;
                                    if let Some(task) = slot.download_task.clone() {
                                        if let Ok(p) = task.progress.try_lock() {
                                            slot.download_progress = *p;
                                        }
                                        if !task.done.load(std::sync::atomic::Ordering::SeqCst) {
                                            slot.stage = LifecycleStage::DownloadingModel;
                                            slot.status = ServiceStatus::Pulling;
                                            continue;
                                        }
                                        let failure = task.failure.lock().await.clone();
                                        slot.download_task = None;
                                        slot.download_progress = None;
                                        if let Some(err) = failure {
                                            warn!(
                                                purpose = %key,
                                                model = %model_id,
                                                "Ollama model pull failed: {err}"
                                            );
                                            slot.status = ServiceStatus::CrashLooping {
                                                restart_count: slot.restart_attempts + 1,
                                            };
                                            slot.stage = LifecycleStage::Failed;
                                            slot.restart_attempts += 1;
                                            emit_crashloop_alert(
                                                &self.db,
                                                row.purpose,
                                                "(native Ollama)",
                                                &format!(
                                                    "Ollama pull of {model_id} failed: {err}"
                                                ),
                                            );
                                            continue;
                                        }
                                        info!(
                                            purpose = %key,
                                            model = %model_id,
                                            "Ollama model pull complete; marking Healthy"
                                        );
                                    } else {
                                        let present =
                                            crate::ollama_puller::is_model_present(
                                                host_port,
                                                &model_id,
                                            )
                                            .await
                                            .unwrap_or_else(|e| {
                                                warn!(
                                                    purpose = %key,
                                                    model = %model_id,
                                                    "/api/tags probe failed ({e}); assuming model absent"
                                                );
                                                false
                                            });
                                        if !present {
                                            info!(
                                                purpose = %key,
                                                model = %model_id,
                                                "Ollama model not in cache; starting /api/pull"
                                            );
                                            let task = spawn_ollama_pull_task(
                                                host_port, &model_id,
                                            );
                                            slot.download_task = Some(task);
                                            slot.stage = LifecycleStage::DownloadingModel;
                                            slot.status = ServiceStatus::Pulling;
                                            continue;
                                        }
                                    }
                                }
                            }
                            slot.status = ServiceStatus::Healthy;
                            slot.stage = LifecycleStage::Healthy;
                            slot.restart_attempts = 0;
                            // Healthy means the previous CrashLooping
                            // diagnostic is no longer relevant —
                            // clear the cached tail so /logs serves
                            // fresh container output, not stale
                            // failure logs.
                            slot.last_log_tail = None;
                            slot.last_log_line = None;
                            resolve_crashloop_alert(&self.db, row.purpose);
                            // Append `/v1` so the persisted endpoint
                            // matches the OpenAI-compatible base-URL
                            // contract every operator-supplied
                            // external row already follows. Without
                            // it, `inference-api` appends
                            // `/chat/completions` to a bare
                            // `http://127.0.0.1:8101` and vLLM
                            // returns 404 — only `/v1/...` paths
                            // are mounted under vLLM's OpenAPI
                            // server. Whisper / Kokoro images use
                            // the same convention.
                            let endpoint = format!("{}/v1", handle.endpoint_url("http"));
                            // Write the URL back so the runner's
                            // next call picks it up. Idempotent —
                            // the row is left alone if the URL
                            // didn't change.
                            if row.endpoint.as_deref() != Some(endpoint.as_str()) {
                                let _ = store.set_endpoint(
                                    row.purpose,
                                    Some(&endpoint),
                                    chrono::Utc::now().timestamp(),
                                );
                            }
                        }
                        Ok(false) => {
                            slot.status = ServiceStatus::Starting;
                            // Promote stage from ContainerStarting
                            // → LoadingModel when the container's
                            // log line announces the model load. We
                            // never demote — once we've seen
                            // "loading model" we stay there until
                            // /health succeeds.
                            if !matches!(slot.stage, LifecycleStage::LoadingModel) {
                                if let Some(promoted) =
                                    classify_stage_from_log(slot.last_log_line.as_deref())
                                {
                                    slot.stage = promoted;
                                } else if matches!(slot.stage, LifecycleStage::PullingImage) {
                                    // We've moved past pull (handle
                                    // is set, container is running)
                                    // even if no log markers yet.
                                    slot.stage = LifecycleStage::ContainerStarting;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(purpose = %key, "health probe error: {e}");
                            slot.status = ServiceStatus::Starting;
                            if !matches!(slot.stage, LifecycleStage::LoadingModel) {
                                if let Some(promoted) =
                                    classify_stage_from_log(slot.last_log_line.as_deref())
                                {
                                    slot.stage = promoted;
                                } else if matches!(slot.stage, LifecycleStage::PullingImage) {
                                    slot.stage = LifecycleStage::ContainerStarting;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Force-restart a single managed backend. Bound to
    /// `POST /api/admin/backends/{purpose}/restart`.
    pub async fn restart(&self, purpose: BackendPurpose) -> Result<(), ServiceError> {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(purpose.as_str()) {
            if let Some(handle) = slot.handle.take() {
                let _ = self.controller.stop(&handle).await;
            }
            slot.status = ServiceStatus::Stopped;
            slot.stage = LifecycleStage::Idle;
            slot.restart_attempts = 0;
            slot.spawn_started_at = None;
            slot.last_log_line = None;
            slot.last_log_tail = None;
            slot.download_progress = None;
            slot.download_task = None;
        }
        drop(slots);
        // Kick so the next reconcile pass spawns immediately.
        self.kick();
        Ok(())
    }

    /// Read-only access to the last `lines` lines of a managed
    /// backend's container logs. When the container is still
    /// present we fetch a fresh tail from the runtime; if it's
    /// been removed (e.g. between CrashLooping cycles) we fall
    /// through to the cached `last_log_tail` the supervisor
    /// captured on the most recent CrashLooping observation.
    ///
    /// Returns `Ok("")` (not Err) when there is genuinely no log
    /// material to show — the route renders that as "no logs yet"
    /// rather than an error toast.
    pub async fn logs(
        &self,
        purpose: BackendPurpose,
        lines: usize,
    ) -> Result<String, ServiceError> {
        let slots = self.slots.lock().await;
        let slot = match slots.get(purpose.as_str()) {
            Some(s) => s.clone(),
            None => return Ok(String::new()),
        };
        drop(slots);
        if let Some(handle) = slot.handle.as_ref() {
            // Live container — let the controller read the
            // runtime tail. If it errors (container removed
            // mid-call, daemon hiccup), fall back to the cached
            // tail rather than failing the whole route.
            match self.controller.tail_logs(handle, lines).await {
                Ok(s) if !s.trim().is_empty() => return Ok(s),
                _ => {}
            }
        }
        Ok(slot.last_log_tail.unwrap_or_default())
    }

    #[allow(dead_code)]
    pub fn db_for_test(&self) -> &Database {
        &self.db
    }
}

// ---------------------------------------------------------------------------
// Alert plumbing — surfaces supervisor failure modes to Settings → Alerts.
// Each managed backend gets a stable fingerprint per `purpose:image` so
// dedup folds repeated reconcile failures into a single Firing row with
// a bumped `occurrence_count`. Auto-resolves on the supervisor's next
// Healthy observation.
// ---------------------------------------------------------------------------

const ALERT_SOURCE: &str = "core.backend_supervisor";

fn alert_fingerprint(purpose: BackendPurpose, image: &str) -> String {
    format!("backend_supervisor:{}:{image}", purpose.as_str())
}

/// Best-effort: a DB hiccup here must NOT block the reconcile loop.
fn emit_crashloop_alert(db: &Database, purpose: BackendPurpose, image: &str, detail: &str) {
    let store = AlertStore::new(db);
    let now = chrono::Utc::now().timestamp();
    let fingerprint = alert_fingerprint(purpose, image);
    let row = AlertRow {
        id: AlertId::new(),
        fingerprint,
        severity: Severity::Error,
        source: ALERT_SOURCE.to_owned(),
        title: format!("{} backend crash-looping", purpose.as_str()),
        detail: Some(detail.to_owned()),
        context_json: None,
        status: AlertStatus::Firing,
        first_seen_at: now,
        last_seen_at: now,
        occurrence_count: 1,
        resolved_at: None,
        resolved_by: None,
        ack_at: None,
        ack_by: None,
        snooze_until: None,
        incident_id: None,
        actions_json: None,
    };
    if let Err(e) = store.insert_firing(&row) {
        warn!(purpose = %purpose.as_str(), "alert insert failed: {e}");
    }
}

/// On a Healthy observation, mark any open Firing alerts for this
/// purpose as Resolved so the operator's badge clears automatically.
fn resolve_crashloop_alert(db: &Database, purpose: BackendPurpose) {
    let store = AlertStore::new(db);
    let prefix = format!("backend_supervisor:{}:", purpose.as_str());
    let firing = store
        .list(Some(&[AlertStatus::Firing]), Some(200))
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    for row in firing.iter().filter(|a| a.fingerprint.starts_with(&prefix)) {
        let _ = store.resolve(&row.id, "supervisor", now);
    }
}

fn spec_image_for_alert(row: &BackendRow) -> String {
    row.model_spec_json
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown image)")
        .to_owned()
}

/// Pull the HF model id from a managed row. Wizard saves args as
/// `["--model=Qwen/Qwen2.5-32B-Instruct-AWQ"]` so we look for the
/// first arg matching `--model=…` or the value after `--model`.
/// Returns `None` for rows that don't ship a model arg (operator
/// hand-edited the JSON, or the spec genuinely doesn't have one);
/// the supervisor then falls back to letting the container handle
/// its own download.
pub(crate) fn extract_model_id(row: &BackendRow) -> Option<String> {
    let args = row.model_spec_json.get("args")?.as_array()?;
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        let s = a.as_str()?;
        if let Some(rest) = s.strip_prefix("--model=") {
            if !rest.is_empty() {
                return Some(rest.to_owned());
            }
        }
        if s == "--model" {
            return iter.next().and_then(|n| n.as_str()).map(|s| s.to_owned());
        }
    }
    None
}

/// Add the host's primary HF cache mount to a spec if the spec
/// doesn't already carry one for `/root/.cache/huggingface`. Also
/// injects `HF_HOME` env so HF libraries inside the container
/// resolve the cache without any extra config.
pub(crate) fn attach_hf_cache_mount(spec: &mut ServiceSpec, primary_cache: &std::path::Path) {
    const CONTAINER_HF: &str = "/root/.cache/huggingface";
    if !spec.mounts.iter().any(|m| m.container_path == CONTAINER_HF) {
        spec.mounts.push(HostMount {
            host_path: primary_cache.to_string_lossy().into_owned(),
            container_path: CONTAINER_HF.to_owned(),
            read_only: true,
        });
    }
    if !spec.env.iter().any(|(k, _)| k == "HF_HOME") {
        spec.env
            .push(("HF_HOME".to_owned(), CONTAINER_HF.to_owned()));
    }
    // Also disable HF's own download attempts inside the container.
    // `HF_HUB_OFFLINE=1` forces transformers/vLLM to read from the
    // mounted cache only; if the file isn't there, they error
    // immediately instead of silently re-downloading (which would
    // defeat the host-side download we just did).
    if !spec.env.iter().any(|(k, _)| k == "HF_HUB_OFFLINE") {
        spec.env.push(("HF_HUB_OFFLINE".to_owned(), "1".to_owned()));
    }
}

/// Detect the Apple-Silicon Ollama-native preset envelope. The
/// wizard's `materialise_spec` writes `runtime: "native"` and
/// `binary_hint: "ollama"` together; both must be present for the
/// supervisor to use the Ollama post-spawn pull path. Future native
/// engines (`service-mlx`, `service-llama-cpp`) will get their own
/// detection helpers as they land.
pub(crate) fn is_ollama_row(row: &BackendRow) -> bool {
    let obj = match row.model_spec_json.as_object() {
        Some(o) => o,
        None => return false,
    };
    let is_native = obj
        .get("runtime")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("native"))
        .unwrap_or(false);
    let is_ollama = obj
        .get("binary_hint")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("ollama"))
        .unwrap_or(false);
    is_native && is_ollama
}

/// Pull the Ollama tag name out of the row. Sibling to
/// `extract_model_id` (which parses HF model ids out of `--model`
/// CLI args for vLLM). Ollama's tag is stored as a dedicated
/// `model` field at the top level of `model_spec_json` — see
/// `materialise_spec` in `backend_presets.rs`.
pub(crate) fn extract_ollama_model_id(row: &BackendRow) -> Option<String> {
    row.model_spec_json
        .get("model")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
}

/// Spawn a background task that calls Ollama's `/api/pull` for
/// `model_id`, mirrors progress into `DownloadTaskState`, and
/// signals `done` on completion. Mirrors `spawn_download_task` but
/// targets the daemon's HTTP API rather than `HfDownloader`.
///
/// `file_idx`/`file_count` are pinned at `(1, 1)` because Ollama's
/// stream reports a single rolling aggregate per layer rather than
/// the per-file breakdown HF gives us. The SPA pill renders the
/// percentage from `bytes_downloaded/total_bytes` and the
/// `1 of 1` line stays informative ("we're pulling one model").
fn spawn_ollama_pull_task(host_port: u16, model_id: &str) -> DownloadTaskState {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let progress: Arc<Mutex<Option<DownloadProgress>>> = Arc::new(Mutex::new(None));
    let model_id_owned = model_id.to_owned();
    let done_clone = done.clone();
    let failure_clone = failure.clone();
    let progress_clone = progress.clone();
    tokio::spawn(async move {
        let result = crate::ollama_puller::pull_model(
            host_port,
            &model_id_owned,
            |completed, total| {
                // try_lock so a contended progress mirror doesn't
                // back-pressure the streaming HTTP body parser.
                // Missing an update is fine — the supervisor reads
                // the latest snapshot on the next reconcile.
                if let Ok(mut guard) = progress_clone.try_lock() {
                    *guard = Some(DownloadProgress {
                        bytes_downloaded: completed,
                        total_bytes: total,
                        file_idx: 1,
                        file_count: 1,
                    });
                }
            },
        )
        .await;
        if let Err(e) = result {
            *failure_clone.lock().await = Some(e.to_string());
        }
        done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    DownloadTaskState {
        model_id: model_id.to_owned(),
        done,
        failure,
        progress,
    }
}

/// Spawn a background task that downloads `model_id` via the host
/// `HfDownloader`, mirrors progress into `DownloadTaskState`, and
/// signals `done` on completion. Returns the state struct so the
/// supervisor can poll it on subsequent reconciles.
fn spawn_download_task(downloader: HfDownloader, model_id: &str) -> DownloadTaskState {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let progress: Arc<Mutex<Option<DownloadProgress>>> = Arc::new(Mutex::new(None));
    let model_id_owned = model_id.to_owned();
    let done_clone = done.clone();
    let failure_clone = failure.clone();
    let progress_clone = progress.clone();
    tokio::spawn(async move {
        let mut stream = match downloader.ensure_model(&model_id_owned).await {
            Ok(s) => s,
            Err(e) => {
                *failure_clone.lock().await = Some(e.to_string());
                done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        while let Some(ev) = stream.next().await {
            match ev {
                DownloadEvent::OverallProgress {
                    bytes_downloaded,
                    total_bytes,
                    file_idx,
                    file_count,
                } => {
                    *progress_clone.lock().await = Some(DownloadProgress {
                        bytes_downloaded,
                        total_bytes,
                        file_idx,
                        file_count,
                    });
                }
                DownloadEvent::Failed { error } => {
                    *failure_clone.lock().await = Some(error);
                }
                DownloadEvent::Complete { .. } => {
                    // Progress freezes at 100% briefly until the
                    // supervisor's next reconcile observes
                    // `done=true` and clears it.
                }
                _ => {}
            }
        }
        done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    DownloadTaskState {
        model_id: model_id.to_owned(),
        done,
        failure,
        progress,
    }
}

/// Trim a log payload to a hard byte budget while keeping the TAIL
/// (which carries the actual error in 99% of cases). Operators want
/// the "why did it crash" line, not the boot banner.
fn tail_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "…(truncated, showing last {} bytes)…\n{}",
        max_bytes,
        &s[start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_container_manager::MockServiceController;
    use execlaw_core::MigrationRunner;
    use execlaw_core::backends::{BackendStore, BackendUpsert};
    use execlaw_core::db::DbConfig;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn ollama_row(model: &str) -> BackendRow {
        // Matches the envelope `materialise_spec` in backend_presets.rs
        // emits for the Apple-Silicon Ollama presets.
        BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-ollama".into(),
            model_spec_json: serde_json::json!({
                "runtime": "native",
                "binary_hint": "ollama",
                "args": ["serve"],
                "container_port": 11434,
                "model": model,
            }),
            gpu_id: Some("apple-m3".into()),
            endpoint: None,
            notes: None,
            reasoning_enabled: true,
            mode: BackendMode::Managed,
            created_at: 0,
            updated_at: 100,
        }
    }

    #[test]
    fn is_ollama_row_detects_apple_native_envelope() {
        let row = ollama_row("qwen2.5:7b-instruct-q4_K_M");
        assert!(is_ollama_row(&row));
    }

    #[test]
    fn is_ollama_row_rejects_vllm_envelope() {
        // A managed vLLM row has `image` + no `runtime` field, so the
        // Ollama gate must NOT fire on it (otherwise the supervisor
        // would try to /api/pull against a Docker container that
        // doesn't expose that endpoint).
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({
                "image": "vllm/vllm-openai:v0.6.2",
                "args": ["--model", "Qwen/Qwen2.5-7B-Instruct-AWQ"],
                "container_port": 8000,
            }),
            gpu_id: Some("0".into()),
            endpoint: None,
            notes: None,
            reasoning_enabled: true,
            mode: BackendMode::Managed,
            created_at: 0,
            updated_at: 0,
        };
        assert!(!is_ollama_row(&row));
    }

    #[test]
    fn extract_ollama_model_id_reads_top_level_model_field() {
        let row = ollama_row("qwen2.5:32b-instruct-q4_K_M");
        assert_eq!(
            extract_ollama_model_id(&row).as_deref(),
            Some("qwen2.5:32b-instruct-q4_K_M")
        );
    }

    #[test]
    fn extract_ollama_model_id_returns_none_for_blank_model_field() {
        // A misconfigured row (model field empty string) must NOT
        // trigger a pull — Ollama would 400 on a blank name.
        let row = ollama_row("");
        assert!(extract_ollama_model_id(&row).is_none());
    }

    fn upsert_managed(
        store: &BackendStore<'_>,
        purpose: BackendPurpose,
        image: &str,
    ) -> BackendRow {
        store
            .upsert(
                &BackendUpsert {
                    purpose,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({
                        "image": image,
                        "args": ["--model", "Qwen3.5-27B-AWQ"],
                        "env": [["HF_HOME", "/cache"]],
                        "container_port": 8000,
                    }),
                    gpu_id: Some("0".into()),
                    endpoint: None,
                    notes: None,
                    reasoning_enabled: true,
                    mode: BackendMode::Managed,
                },
                100,
            )
            .unwrap()
    }

    #[test]
    fn health_path_picks_ollama_api_tags_for_native_ollama() {
        let spec = ServiceSpec {
            binary_hint: "ollama".into(),
            runtime: execlaw_container_manager::ServiceRuntime::Native,
            ..Default::default()
        };
        assert_eq!(health_path_for(&spec), "/api/tags");
    }

    #[test]
    fn health_path_picks_slash_health_for_docker_vllm() {
        let spec = ServiceSpec {
            image: "vllm/vllm-openai:nightly".into(),
            binary_hint: String::new(),
            ..Default::default()
        };
        assert_eq!(health_path_for(&spec), "/health");
    }

    #[test]
    fn health_path_picks_slash_health_for_unknown_hint() {
        // Defensive: a future preset that types the wrong hint in
        // model_spec_json shouldn't accidentally probe `/api/tags`
        // (which would 404 on a vLLM and demote a healthy backend).
        let spec = ServiceSpec {
            binary_hint: "made-up-engine".into(),
            ..Default::default()
        };
        assert_eq!(health_path_for(&spec), "/health");
    }

    #[test]
    fn parse_gpu_vendor_recognises_apple() {
        assert_eq!(parse_gpu_vendor("apple"), Some(GpuVendor::Apple));
        assert_eq!(parse_gpu_vendor("Apple"), Some(GpuVendor::Apple));
    }

    #[test]
    fn host_port_is_stable_per_purpose() {
        // The runner relies on these being deterministic across
        // restarts so a routine that ran yesterday doesn't see a
        // different URL today.
        for p in [
            BackendPurpose::Standard,
            BackendPurpose::Small,
            BackendPurpose::VoiceStt,
            BackendPurpose::VoiceTts,
        ] {
            assert_eq!(host_port_for(p), host_port_for(p));
        }
        // No two purposes share a port.
        let mut ports = std::collections::HashSet::new();
        for p in [
            BackendPurpose::Standard,
            BackendPurpose::Small,
            BackendPurpose::VoiceStt,
            BackendPurpose::VoiceTts,
        ] {
            assert!(
                ports.insert(host_port_for(p)),
                "port collision for {}",
                p.as_str()
            );
        }
    }

    #[test]
    fn inject_tool_args_appends_when_missing_on_vllm_image() {
        let original = vec!["--model=Qwen3.5".into()];
        let mut out = original.clone();
        inject_required_vllm_tool_args("vllm/vllm-openai:nightly", &original, &mut out);
        assert!(out.contains(&"--enable-auto-tool-choice".into()));
        // Qwen3 + Qwen3.5 → qwen3_xml (the model emits XML-shaped
        // tool calls, not Hermes JSON).
        assert!(out.contains(&"--tool-call-parser=qwen3_xml".into()));
        // Prefix caching is mandatory for tool-loop performance —
        // see the doc comment on inject_required_vllm_tool_args.
        assert!(out.contains(&"--enable-prefix-caching".into()));
    }

    #[test]
    fn inject_tool_args_picks_qwen3_coder_for_qwen3_6_models() {
        // 2026-05-12 — Qwen3.6 uses the `qwen3_coder` tool-call
        // parser per the vLLM Recipes guide. Distinct from Qwen3 /
        // Qwen3.5 which use `qwen3_xml`. Test both common spellings
        // of the model id ("3.6" and the underscore variant some
        // mirror repos use).
        for model in [
            "--model=QuantTrio/Qwen3.6-27B-AWQ",
            "--model=Qwen/Qwen3.6-27B",
            "--model=some/Qwen3_6-MoE",
        ] {
            let original = vec![model.into()];
            let mut out = original.clone();
            inject_required_vllm_tool_args("vllm/vllm-openai:v0.20.2", &original, &mut out);
            assert!(
                out.contains(&"--tool-call-parser=qwen3_coder".into()),
                "expected qwen3_coder parser for {model}, got: {out:?}"
            );
        }
    }

    #[test]
    fn inject_tool_args_respects_existing_operator_overrides() {
        // If the operator hand-set the parser (e.g. to llama3_json),
        // we leave it alone and don't double up. Same goes for
        // --no-enable-prefix-caching (operator opt-out).
        let original = vec![
            "--model=meta-llama/Llama-3-8B".into(),
            "--enable-auto-tool-choice".into(),
            "--tool-call-parser=llama3_json".into(),
            "--no-enable-prefix-caching".into(),
        ];
        let mut out = original.clone();
        inject_required_vllm_tool_args("vllm/vllm-openai:nightly", &original, &mut out);
        assert_eq!(out.len(), 4, "no duplicates added: {out:?}");
    }

    #[test]
    fn inject_tool_args_skips_non_vllm_images() {
        // Whisper STT / Kokoro TTS shouldn't get LLM tool flags.
        let original = vec!["--model=base.en".into()];
        let mut out = original.clone();
        inject_required_vllm_tool_args(
            "ghcr.io/fedirz/faster-whisper-server:latest",
            &original,
            &mut out,
        );
        assert_eq!(out, original);
    }

    #[test]
    fn parser_default_picks_llama_for_llama_models() {
        assert_eq!(
            default_tool_parser_for_args(&["--model=meta-llama/Llama-3-8B".into()]),
            "llama3_json",
        );
        assert_eq!(
            default_tool_parser_for_args(
                &["--model".into(), "meta-llama/Meta-Llama-3-70B".into(),]
            ),
            "llama3_json",
        );
    }

    #[test]
    fn parser_default_picks_qwen3_xml_for_qwen3_models() {
        // Qwen3.5-AWQ emits the native XML format vLLM ships a
        // dedicated parser for. Hermes silently fails on it
        // (sets finish_reason=tool_calls but extracts zero calls).
        assert_eq!(
            default_tool_parser_for_args(&["--model=QuantTrio/Qwen3.5-27B-AWQ".into()]),
            "qwen3_xml",
        );
        assert_eq!(
            default_tool_parser_for_args(&["--model=Qwen/Qwen3-32B".into()]),
            "qwen3_xml",
        );
    }

    #[test]
    fn parser_default_picks_hermes_for_qwen2_and_unknown() {
        assert_eq!(
            default_tool_parser_for_args(&["--model=Qwen/Qwen2.5-3B-Instruct-AWQ".into()]),
            "hermes",
        );
        assert_eq!(
            default_tool_parser_for_args(&["--model=mystery/model".into()]),
            "hermes",
        );
    }

    #[test]
    fn spec_from_row_extracts_image_args_env() {
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({
                "image": "vllm/vllm-openai:v0.6.2",
                "args": ["--model", "Qwen3.5-27B-AWQ", "--gpu-memory-utilization", "0.9"],
                "env": [["HF_HOME", "/cache"], ["NO_PROXY", "localhost"]],
                "container_port": 8000,
            }),
            gpu_id: Some("0".into()),
            endpoint: None,
            notes: None,
            reasoning_enabled: true,
            mode: BackendMode::Managed,
            created_at: 100,
            updated_at: 100,
        };
        let spec = spec_from_row(&row).unwrap();
        assert_eq!(spec.image, "vllm/vllm-openai:v0.6.2");
        // Original 4 args + three injected by
        // `inject_required_vllm_tool_args`: --enable-auto-tool-choice,
        // --tool-call-parser=qwen3_xml, --enable-prefix-caching.
        assert_eq!(spec.args.len(), 7);
        assert!(spec.args.contains(&"--enable-auto-tool-choice".into()));
        // Qwen3.5 → qwen3_xml (see default_tool_parser_for_args).
        assert!(spec.args.contains(&"--tool-call-parser=qwen3_xml".into()));
        assert!(spec.args.contains(&"--enable-prefix-caching".into()));
        assert_eq!(spec.env.len(), 2);
        assert_eq!(spec.host_port, host_port_for(BackendPurpose::Standard));
        assert_eq!(spec.container_port, 8000);
        assert_eq!(spec.gpu_id.as_deref(), Some("0"));
    }

    #[test]
    fn spec_from_row_routes_apple_preset_to_native_runtime() {
        // End-to-end Phase 4 wiring: a row matching what the Apple
        // preset library writes (no `image`, `runtime: "native"`,
        // `binary_hint: "ollama"`) must round-trip through
        // `spec_from_row` into a Native-runtime ServiceSpec the
        // multiplexed controller can dispatch.
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-ollama".into(),
            model_spec_json: serde_json::json!({
                "runtime": "native",
                "binary_hint": "ollama",
                "args": ["serve"],
                "container_port": 11434,
                "model": "qwen2.5:32b-instruct-q4_K_M",
            }),
            gpu_id: None,
            endpoint: None,
            notes: None,
            reasoning_enabled: true,
            mode: BackendMode::Managed,
            created_at: 100,
            updated_at: 100,
        };
        let spec = spec_from_row(&row).expect("native preset row must round-trip");
        assert_eq!(
            spec.runtime,
            execlaw_container_manager::ServiceRuntime::Native
        );
        assert_eq!(spec.binary_hint, "ollama");
        assert_eq!(spec.container_port, 11434);
        assert!(spec.image.is_empty());
        // The supervisor's health probe will hit /api/tags via
        // health_path_for; sanity-check that helper agrees on this
        // spec shape so the integration is end-to-end.
        assert_eq!(health_path_for(&spec), "/api/tags");
        // Ollama-args injection: `serve` is the only thing the
        // supervisor needs; no vLLM tool-call args should sneak in.
        assert_eq!(spec.args, vec!["serve".to_owned()]);
    }

    #[test]
    fn spec_from_row_docker_runtime_still_requires_image() {
        // Regression guard: relaxing `image` for native rows must
        // NOT relax it for the default Docker path. A typo'd Docker
        // row (image missing) must still fail spec_from_row so the
        // supervisor doesn't try to spawn a no-image container.
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({
                // No `runtime` field → defaults to Docker.
                "args": ["--model", "X"],
            }),
            gpu_id: None,
            endpoint: None,
            notes: None,
            reasoning_enabled: false,
            mode: BackendMode::Managed,
            created_at: 0,
            updated_at: 0,
        };
        assert!(spec_from_row(&row).is_err());
    }

    #[test]
    fn spec_from_row_rejects_missing_image() {
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({"args": ["--model", "X"]}),
            gpu_id: None,
            endpoint: None,
            notes: None,
            reasoning_enabled: false,
            mode: BackendMode::Managed,
            created_at: 0,
            updated_at: 0,
        };
        assert!(spec_from_row(&row).is_err());
    }

    #[tokio::test]
    async fn reconcile_spawns_managed_row_and_writes_endpoint() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");

        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db.clone(), mock.clone());
        // First pass: spawn (mock spawn -> running). Then health
        // probes default to true → Healthy. Then endpoint is
        // written back.
        sup.reconcile_once().await;
        sup.reconcile_once().await;

        let row = store.get(BackendPurpose::Standard).unwrap().unwrap();
        assert_eq!(
            row.endpoint.as_deref(),
            // `/v1` suffix matches the OpenAI-base-URL contract the
            // inference-api client expects.
            Some(
                format!(
                    "http://127.0.0.1:{}/v1",
                    host_port_for(BackendPurpose::Standard)
                )
                .as_str()
            )
        );
        assert_eq!(mock.spawn_count().await, 1);

        let snap = sup.snapshot_status().await;
        let standard = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(standard.status, ServiceStatus::Healthy);
    }

    #[tokio::test]
    async fn reconcile_skips_external_rows() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        // External row — supervisor must NOT spawn.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://192.168.1.50:8000/v1".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                100,
            )
            .unwrap();

        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 0);
    }

    #[tokio::test]
    async fn reconcile_stops_container_when_row_flips_to_external() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db.clone(), mock.clone());

        // First two passes: spawn + probe → endpoint written.
        sup.reconcile_once().await;
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 1);

        // Operator flips the row to external (e.g. switches to a
        // remote vLLM). Supervisor must stop the container and
        // clear the endpoint.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://remote/v1".into()),
                    notes: None,
                    reasoning_enabled: true,
                    mode: BackendMode::External,
                },
                200,
            )
            .unwrap();
        sup.reconcile_once().await;
        assert_eq!(mock.stop_count().await, 1);

        let row = store.get(BackendPurpose::Standard).unwrap().unwrap();
        // Supervisor cleared its supervised endpoint; the new
        // external endpoint the operator typed survives the upsert
        // (this is the operator's data, not ours to clear after).
        // The supervisor's stop flow only clears endpoint when the
        // mode flip is observed BEFORE the upsert overwrites it —
        // here the upsert provided a new endpoint, which wins.
        assert_eq!(row.endpoint.as_deref(), Some("http://remote/v1"));
    }

    #[tokio::test]
    async fn reconcile_marks_crash_looping_after_repeated_failures() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:bad");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());

        // First spawn lands fine, but pin the inspect to
        // CrashLooping so every reconcile re-attempts.
        sup.reconcile_once().await;
        mock.pin_status(ServiceStatus::CrashLooping { restart_count: 5 })
            .await;
        for _ in 0..6 {
            sup.reconcile_once().await;
        }

        let snap = sup.snapshot_status().await;
        let s = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        match &s.status {
            ServiceStatus::CrashLooping { .. } => {} // expected
            other => panic!("expected CrashLooping, got {other:?}"),
        }
        assert!(
            s.restart_attempts >= MAX_RESTART_ATTEMPTS,
            "should have hit the cap; got {}",
            s.restart_attempts
        );
    }

    #[tokio::test]
    async fn restart_drops_handle_and_kicks() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());

        sup.reconcile_once().await;
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 1);

        sup.restart(BackendPurpose::Standard).await.unwrap();
        // After restart, the slot should drop the handle so the
        // next reconcile spawns again.
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 2);
        assert_eq!(mock.stop_count().await, 1);
    }

    #[tokio::test]
    async fn spawn_error_increments_restart_attempts_and_records_crash_looping() {
        // Closure for Phase 12 audit gap #4: when the controller's
        // `spawn` returns Err (e.g. image pull failed), the
        // supervisor must mark the slot CrashLooping and bump the
        // restart counter so the cap eventually engages instead of
        // looping forever.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:nope");
        let mock = Arc::new(MockServiceController::new());
        mock.pin_spawn_pull_error("registry 404").await;
        let sup = BackendSupervisor::new(db, mock.clone());

        for _ in 0..3 {
            sup.reconcile_once().await;
        }
        let snap = sup.snapshot_status().await;
        let row = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        match &row.status {
            ServiceStatus::CrashLooping { .. } => {} // expected
            other => panic!("expected CrashLooping after spawn errors, got {other:?}"),
        }
        assert!(
            row.restart_attempts > 0,
            "spawn-error path must increment restart_attempts; got {}",
            row.restart_attempts
        );
        // Verify spawn was attempted at least once (not silently
        // suppressed) — actual count varies because once the cap
        // hits the supervisor stops attempting.
        assert!(mock.spawn_count().await >= 1);
    }

    #[tokio::test]
    async fn reset_attempts_unparks_a_crashlooping_row() {
        // Phase 12 closure — after the supervisor parks a row at
        // MAX_RESTART_ATTEMPTS, the upsert handler calls
        // reset_attempts so the next reconcile makes a fresh
        // determination. Without this, the only way to recover
        // from a crash loop was a server restart.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:bad");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());

        sup.reconcile_once().await;
        mock.pin_status(ServiceStatus::CrashLooping { restart_count: 5 })
            .await;
        for _ in 0..6 {
            sup.reconcile_once().await;
        }

        // Verify we're parked.
        let snap = sup.snapshot_status().await;
        let parked = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert!(parked.restart_attempts >= MAX_RESTART_ATTEMPTS);

        // Operator's "save" → reset_attempts() → kick. Drop the
        // pinned status so the mock's next inspect reports the
        // running container as Healthy again.
        sup.reset_attempts(BackendPurpose::Standard).await;
        // Clear the pin so subsequent inspects fall through to the
        // mock's default (Healthy when running). We do this by
        // pinning Healthy explicitly.
        mock.pin_status(ServiceStatus::Healthy).await;

        sup.reconcile_once().await;
        sup.reconcile_once().await;
        let snap2 = sup.snapshot_status().await;
        let unstuck = snap2
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(
            unstuck.restart_attempts, 0,
            "reset_attempts must clear the counter"
        );
        assert_eq!(unstuck.status, ServiceStatus::Healthy);
    }

    #[tokio::test]
    async fn snapshot_status_lists_external_as_stopped_and_managed_as_observed() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        // External Small + managed Standard.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Small,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://x".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                100,
            )
            .unwrap();
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");

        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock);
        sup.reconcile_once().await;
        sup.reconcile_once().await;

        let snap = sup.snapshot_status().await;
        let small = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Small)
            .unwrap();
        assert_eq!(small.mode, BackendMode::External);
        // External rows are always Stopped from the supervisor's POV.
        assert_eq!(small.status, ServiceStatus::Stopped);

        let standard = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(standard.mode, BackendMode::Managed);
        assert_eq!(standard.status, ServiceStatus::Healthy);
    }

    // ---- Phase 14.B — lifecycle stage / elapsed / log-line ----------

    #[test]
    fn classify_stage_recognises_vllm_loading_markers() {
        // Real lines that vLLM emits on stdout during model load.
        // Each one MUST flip the stage to LoadingModel so the SPA
        // pill stops saying "Provisioning" forever.
        for line in &[
            "INFO 04-28 02:36:35 [gpu_model_runner.py] Starting to load model QuantTrio/Qwen3.5-27B-AWQ...",
            "Loading safetensors checkpoint shards: 0% Complete",
            "Loading checkpoint shards: 33%",
            "Capturing CUDA graph shapes for warmup batch 4",
            "Loading weights took 22.34 seconds",
        ] {
            let s = classify_stage_from_log(Some(line));
            assert_eq!(
                s,
                Some(LifecycleStage::LoadingModel),
                "line should classify as LoadingModel: {line}"
            );
        }

        // Lines that are NOT model-loading markers stay None — the
        // caller keeps the previous stage rather than misreading
        // unrelated chatter.
        for line in &[
            "INFO 04-28 02:36:35 [parallel_state.py] world_size=1 rank=0",
            "[transformers] use_fast parameter is deprecated",
            "",
        ] {
            let s = classify_stage_from_log(Some(line));
            assert_eq!(s, None, "line should not classify: {line}");
        }
    }

    #[tokio::test]
    async fn snapshot_reports_stage_elapsed_and_log_line() {
        // Phase 14.B — the SPA's pill consumes `stage`,
        // `elapsed_secs`, and `last_log_line`. Verify each fields
        // is populated end-to-end through reconcile + snapshot.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");
        let mock = Arc::new(MockServiceController::new());
        mock.pin_logs(
            "execlaw-backend-Standard",
            "INFO 02:36:35 Starting to load model QuantTrio/Qwen3.5-27B-AWQ\n",
        )
        .await;
        // Pin health=false so the loop sits in the Starting branch
        // long enough for us to observe LoadingModel.
        mock.pin_health(false).await;
        let sup = BackendSupervisor::new(db.clone(), mock.clone());

        sup.reconcile_once().await; // spawn
        sup.reconcile_once().await; // probe → Starting (loading)

        let snap = sup.snapshot_status().await;
        let s = snap
            .iter()
            .find(|x| x.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(s.stage, LifecycleStage::LoadingModel);
        assert!(
            s.elapsed_secs.is_some(),
            "elapsed_secs must populate after spawn"
        );
        assert!(
            s.last_log_line
                .as_deref()
                .unwrap_or("")
                .contains("Starting to load model"),
            "last_log_line must surface the most recent line; got {:?}",
            s.last_log_line
        );

        // Now flip the mock to healthy. Next reconcile should
        // promote stage → Healthy + clear last_log_line (the
        // CrashLooping diagnostic isn't relevant once the service
        // is up).
        mock.pin_health(true).await;
        sup.reconcile_once().await;
        let snap2 = sup.snapshot_status().await;
        let s2 = snap2
            .iter()
            .find(|x| x.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(s2.stage, LifecycleStage::Healthy);
        assert_eq!(s2.last_log_line, None);
        assert_eq!(s2.status, ServiceStatus::Healthy);
    }

    #[test]
    fn extract_model_id_handles_eq_form_and_separate_arg() {
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({
                "image": "vllm/vllm-openai:nightly",
                "args": ["--model=Qwen/Qwen2.5-32B-Instruct-AWQ", "--gpu-memory-utilization=0.9"],
                "container_port": 8000,
            }),
            gpu_id: Some("0".into()),
            endpoint: None,
            notes: None,
            reasoning_enabled: true,
            mode: BackendMode::Managed,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(
            extract_model_id(&row).as_deref(),
            Some("Qwen/Qwen2.5-32B-Instruct-AWQ")
        );

        // Separate-arg form: `["--model", "<id>"]`.
        let row2 = BackendRow {
            model_spec_json: serde_json::json!({
                "image": "vllm:nightly",
                "args": ["--model", "QuantTrio/Qwen3.5-27B-AWQ"],
            }),
            ..row.clone()
        };
        assert_eq!(
            extract_model_id(&row2).as_deref(),
            Some("QuantTrio/Qwen3.5-27B-AWQ")
        );

        // No model arg → None.
        let row3 = BackendRow {
            model_spec_json: serde_json::json!({
                "image": "vllm:nightly",
                "args": ["--gpu-memory-utilization=0.9"],
            }),
            ..row.clone()
        };
        assert_eq!(extract_model_id(&row3), None);
    }

    #[test]
    fn attach_hf_cache_mount_is_idempotent_and_sets_offline_env() {
        // Phase 14.C — when the supervisor wires the host HF cache
        // mount into a spec, it must:
        //   * not duplicate an existing /root/.cache/huggingface mount
        //   * not duplicate HF_HOME / HF_HUB_OFFLINE env vars
        //   * set both env vars when adding for the first time, so
        //     transformers/vLLM read from the mount (HF_HOME) and
        //     refuse to phone home (HF_HUB_OFFLINE=1) when a file's
        //     missing — the host-side downloader is the only writer.
        use std::path::PathBuf;
        let mut spec = ServiceSpec {
            name: "execlaw-backend-Standard".into(),
            image: "vllm:nightly".into(),
            gpu_id: Some("0".into()),
            gpu_vendor: Some(GpuVendor::Nvidia),
            host_port: 8101,
            container_port: 8000,
            ..Default::default()
        };
        let cache = PathBuf::from("/home/me/.execlaw/hf-cache");
        attach_hf_cache_mount(&mut spec, &cache);
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.mounts[0].container_path, "/root/.cache/huggingface");
        assert!(spec.mounts[0].read_only);
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "HF_HOME" && v == "/root/.cache/huggingface")
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "HF_HUB_OFFLINE" && v == "1")
        );

        // Calling again is a no-op (idempotent).
        attach_hf_cache_mount(&mut spec, &cache);
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.env.iter().filter(|(k, _)| k == "HF_HOME").count(), 1);
        assert_eq!(
            spec.env
                .iter()
                .filter(|(k, _)| k == "HF_HUB_OFFLINE")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn supervisor_adopts_existing_running_container_on_first_reconcile() {
        // Phase 14.E regression — every binary restart used to
        // force-remove the running container by name and spawn a
        // brand-new one. With 27 GB Qwen 3.5 weights, that meant
        // a fresh 3-5 minute model reload on every cargo-watch
        // rebuild — the "5-minute restart loop" the operator saw.
        //
        // Setup: pre-populate the mock controller with a container
        // for `execlaw-backend-Standard` (simulating "previous
        // binary instance left this running"). Then build a fresh
        // supervisor (empty slots map) and run reconcile_once.
        // Expect: handle adopted, NO new spawn, container survives.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");

        let mock = Arc::new(MockServiceController::new());
        // Pretend a container already exists. We seed `running` by
        // calling spawn once on a separate spec — that's the most
        // direct way to put the mock into the "container present"
        // state without exposing internals.
        let pre_spec = ServiceSpec {
            name: "execlaw-backend-Standard".into(),
            image: "vllm:test".into(),
            args: vec!["--model".into(), "X".into()],
            gpu_id: Some("0".into()),
            host_port: 8101,
            container_port: 8000,
            ..Default::default()
        };
        let pre_handle = mock.spawn(&pre_spec).await.unwrap();
        let pre_spawn_count = mock.spawn_count().await;

        // Fresh supervisor — empty slots, simulating a binary restart.
        let sup = BackendSupervisor::new(db.clone(), mock.clone());
        sup.reconcile_once().await;

        // The supervisor must NOT have called spawn again — the
        // existing container was adopted in place. Mock health
        // defaults to true so the second reconcile flips to Healthy
        // without ever respawning.
        assert_eq!(
            mock.spawn_count().await,
            pre_spawn_count,
            "supervisor must adopt existing container, not respawn"
        );

        // Subsequent reconcile should observe Healthy (mock probe
        // returns true; the adopted handle inspects as Starting,
        // which the probe-success branch flips to Healthy).
        sup.reconcile_once().await;
        let snap = sup.snapshot_status().await;
        let standard = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(standard.status, ServiceStatus::Healthy);
        assert_eq!(mock.stop_count().await, 0, "no stops should have fired");

        // The adopted handle's name + host_port match what the
        // supervisor expects (so endpoint_url stays stable).
        assert_eq!(pre_handle.name, "execlaw-backend-Standard");
    }

    #[tokio::test]
    async fn supervisor_does_not_adopt_a_stopped_container() {
        // Defensive — if the existing container is stopped (crashed
        // last time, or operator paused it), adoption returns None
        // and the supervisor proceeds with a clean spawn that
        // force-removes the stale carcass. Critical: a stopped
        // container in the way of the new name binding would HTTP
        // 409 the create_container call.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");

        let mock = Arc::new(MockServiceController::new());
        // Seed + immediately stop, so `running` map is empty but
        // a name has been seen (the mock collapses these into the
        // same "no running container" state, which is exactly what
        // the supervisor should treat as "spawn fresh").
        let spec = ServiceSpec {
            name: "execlaw-backend-Standard".into(),
            image: "vllm:test".into(),
            gpu_id: Some("0".into()),
            host_port: 8101,
            container_port: 8000,
            ..Default::default()
        };
        let h = mock.spawn(&spec).await.unwrap();
        mock.stop(&h).await.unwrap();
        let baseline = mock.spawn_count().await;

        let sup = BackendSupervisor::new(db, mock.clone());
        sup.reconcile_once().await;
        // Supervisor noticed no live container → spawned fresh.
        assert!(
            mock.spawn_count().await > baseline,
            "supervisor must spawn fresh when no live container exists"
        );
    }

    #[tokio::test]
    async fn restart_clears_stage_and_elapsed() {
        // Operator-triggered restart should reset every per-spawn
        // field so the SPA pill doesn't show stale "Healthy ·
        // 4m20s" while the new container is booting.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock);
        sup.reconcile_once().await;
        sup.reconcile_once().await;
        let before = sup.snapshot_status().await;
        let s_before = before
            .iter()
            .find(|x| x.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(s_before.stage, LifecycleStage::Healthy);
        assert!(s_before.elapsed_secs.is_some());

        sup.restart(BackendPurpose::Standard).await.unwrap();
        let after = sup.snapshot_status().await;
        let s_after = after
            .iter()
            .find(|x| x.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(s_after.stage, LifecycleStage::Idle);
        assert_eq!(s_after.elapsed_secs, None);
    }
}
