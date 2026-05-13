//! Phase 13.B.1 — managed-backend preset library.
//!
//! Operators configure a managed backend (Phase 12.D) by typing
//! `model_spec_json` — image tag, args, env, container_port. Presets
//! turn that into "click the right card." The SPA's BackendWizard
//! fetches per-purpose presets from `/api/admin/backends/presets`,
//! recommends the one that matches the detected GPU, lets the
//! operator pick a configurable knob (Whisper model size today),
//! and submits a fully-formed `model_spec_json`.
//!
//! Preset table is locked in code rather than schema-driven so the
//! shipping defaults are deterministic + reviewable. Future per-
//! plugin presets land via plugin manifests when the plugin host
//! grows that surface; for v1 we cover every host that has a
//! runnable preset for its purpose — the matrix is sparse (vLLM
//! ships NVIDIA + CPU; Whisper ships NVIDIA + Intel Arc + CPU;
//! Kokoro ships NVIDIA + Intel Arc; Piper covers the CPU-only
//! voice-tts fallback) but every `BackendPurpose` has at least one
//! preset for every host class.
//!
//! Image references for the execlaw-built service-* images are
//! intentionally placeholders — the operator overrides via the
//! wizard's "Show advanced" disclosure when their registry differs.
//! The vLLM GPU image (`vllm/vllm-openai`) is a real public tag.
//! There is no published vLLM CPU image — the CPU-fallback presets
//! reference an `execlaw/service-vllm-cpu` placeholder the
//! deployment is responsible for building from `Dockerfile.cpu`.
//!
//! Each preset carries an `inference_backend` (PluginId) so the SPA
//! never has to guess from the preset id; the server is the single
//! source of truth for both image+args and the matching plugin.

use execlaw_container_manager::GpuVendor;
use execlaw_core::backends::BackendPurpose;
use serde::Serialize;
use utoipa::ToSchema;

/// Where this preset wants to run. Mirrors `GpuVendor` but with an
/// explicit "Cpu" variant for the no-GPU fallback (`GpuVendor::Unknown`
/// would be ambiguous — Unknown means "we couldn't tell," not "no
/// GPU was requested").
///
/// `Apple` is a special case: there's no Metal-to-container surface,
/// so Apple presets always materialise into the native-runtime
/// envelope (`runtime: "native"`) rather than a Docker image. See
/// [`materialise_spec`] for the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresetVendor {
    Nvidia,
    Intel,
    Apple,
    Cpu,
}

impl PresetVendor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nvidia => "nvidia",
            Self::Intel => "intel",
            Self::Apple => "apple",
            Self::Cpu => "cpu",
        }
    }
}

/// One configurable knob a preset exposes. v1 only ships `model_size`
/// for Whisper, but the shape is extensible — future fields like
/// `gpu_memory_utilization`, `temperature`, etc. land by adding new
/// variants without breaking existing presets.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PresetField {
    /// Discriminator for the SPA's renderer ("model_size", "gpu_id", ...).
    pub kind: String,
    /// Human label shown next to the form control.
    pub label: String,
    /// Allowed values shown in a dropdown.
    pub choices: Vec<String>,
    /// Initial selection.
    pub default: String,
    /// String template substituted into the container's CMD args
    /// when the preset is materialised. `{value}` is replaced with
    /// the operator's selection. Empty when the field doesn't
    /// produce a CMD arg (e.g. the SPA shows it but bakes it
    /// elsewhere).
    pub arg_template: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BackendPreset {
    /// Stable id used by the SPA to reference the chosen preset on
    /// save. Example: `whisper-faster-cuda`.
    pub id: String,
    /// Which BackendPurpose this preset belongs to.
    pub purpose: String,
    /// PluginId of the inference plugin that runs this preset. The
    /// SPA writes this verbatim into `inference_backend` on save —
    /// no guessing from the preset id. Example: `service-vllm`,
    /// `service-whisper-stt`, `service-kokoro-tts`, `service-piper-tts`.
    pub inference_backend: String,
    /// Human-friendly display name. Example: "faster-whisper (NVIDIA)".
    pub name: String,
    /// One-sentence description for the wizard card.
    pub description: String,
    /// Container image reference. Operator can override via the
    /// wizard's advanced disclosure if they need a private registry
    /// or pinned digest.
    pub image: String,
    /// Port the container listens on internally. Maps to
    /// `ServiceSpec.container_port` (Phase 12.B).
    pub container_port: u16,
    /// `nvidia` | `intel` | `cpu` — the SPA marks the matching
    /// preset as `recommended: true` based on detected hardware.
    pub vendor: String,
    /// Static cmd args. Configurable fields produce additional
    /// args via their `arg_template` at materialisation time.
    pub default_args: Vec<String>,
    /// Configurable knobs (e.g. Whisper model size).
    pub fields: Vec<PresetField>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PresetWithFlag {
    #[serde(flatten)]
    pub preset: BackendPreset,
    /// True when the preset's vendor matches the host's detected
    /// hardware. The SPA highlights recommended presets.
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PresetsResponse {
    pub purpose: String,
    /// Detected GPU vendors on the host. Drives which presets are
    /// flagged `recommended`. Empty when no GPU was detected, in
    /// which case the CPU preset (where one exists) is recommended.
    pub detected_vendors: Vec<String>,
    pub presets: Vec<PresetWithFlag>,
}

// ---------------------------------------------------------------------------
// Locked preset library
// ---------------------------------------------------------------------------

/// Whisper model sizes per the upstream Whisper repo. The SPA
/// dropdown surfaces all five; `small` is the default because it
/// hits the locked decisions table sweet spot of accuracy vs
/// latency for English at 16kHz.
const WHISPER_MODEL_SIZES: &[&str] = &["tiny", "base", "small", "medium", "large-v3"];
const WHISPER_DEFAULT_SIZE: &str = "small";

fn whisper_model_size_field() -> PresetField {
    PresetField {
        kind: "model_size".into(),
        label: "Model size".into(),
        choices: WHISPER_MODEL_SIZES
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        default: WHISPER_DEFAULT_SIZE.into(),
        arg_template: "--model-size={value}".into(),
    }
}

/// Ollama-registry model choices for the Apple-Silicon presets.
/// Ollama tags differ from HF — these are the names `ollama pull`
/// understands. The supervisor reads the selected value out of
/// `model_spec_json.model` and issues `POST /api/pull` before
/// marking the backend healthy.
///
/// Qwen3.5 isn't published to the Ollama registry yet (as of
/// 2026-05); we pin Qwen2.5-instruct in the same parameter sizes
/// the NVIDIA presets target so the operator-visible model menu
/// stays predictable. Operators can override via the wizard's
/// advanced disclosure.
fn ollama_model_field(default_model: &str) -> PresetField {
    PresetField {
        kind: "model".into(),
        label: "Model".into(),
        choices: vec![
            "qwen2.5:32b-instruct-q4_K_M".into(),
            "qwen2.5:14b-instruct-q4_K_M".into(),
            "qwen2.5:7b-instruct-q4_K_M".into(),
            "qwen2.5:3b-instruct-q4_K_M".into(),
        ],
        default: default_model.into(),
        // Empty template: the model name is not a CLI arg to
        // `ollama serve`. The supervisor reads it from
        // `model_spec_json.model` and feeds it to `/api/pull`.
        arg_template: "".into(),
    }
}

fn vllm_model_field(default_model: &str, label: &str) -> PresetField {
    PresetField {
        kind: "model".into(),
        label: label.into(),
        choices: vec![
            // Qwen3.6-27B-AWQ is the Standard default (2026-05-12
            // — superseded the prior Qwen3.5 lock; 3.6 ships
            // explicit agentic-coding improvements and uses the
            // `qwen3_coder` tool-call parser in vLLM v0.19+).
            // Qwen3.5 stays in the picker as a fallback for hosts
            // that can't run 3.6 (e.g. older GPU drivers).
            "QuantTrio/Qwen3.6-27B-AWQ".into(),
            "QuantTrio/Qwen3.5-27B-AWQ".into(),
            "Qwen/Qwen2.5-3B-Instruct-AWQ".into(),
        ],
        default: default_model.into(),
        arg_template: "--model={value}".into(),
    }
}

/// Returns every preset for the given purpose. Recommendations come
/// from `presets_for` which annotates each entry with the matching
/// `recommended` flag.
pub fn all_presets() -> Vec<BackendPreset> {
    vec![
        // ---------- Standard (LLM) ----------
        BackendPreset {
            id: "vllm-cuda".into(),
            purpose: BackendPurpose::Standard.as_str().to_owned(),
            inference_backend: "service-vllm".into(),
            name: "vLLM (NVIDIA)".into(),
            description: "OpenAI-compatible vLLM server on NVIDIA. Default model is Qwen3.6-27B-AWQ. Pinned to v0.20.2 (May 10 2026) — Qwen3.6 needs vLLM >= 0.19.0 + the `qwen3_coder` tool-call parser per the vLLM Recipes guide. The `nightly` tag was previously used when Qwen3.5 was unreleased; nightly is now actively risky (operator hit a hang in 0.20.2rc1.dev209 where vLLM accepted a request and never produced a single decode token).".into(),
            image: "vllm/vllm-openai:v0.20.2".into(),
            container_port: 8000,
            vendor: PresetVendor::Nvidia.as_str().to_owned(),
            // Tool-call args are required: the runner ALWAYS sends
            // `tools: [...]` (built-in tools register at boot) and
            // vLLM 400s without these. The supervisor also injects
            // them defensively at launch — listing them here keeps
            // the SPA's "advanced args" view accurate.
            default_args: vec![
                "--gpu-memory-utilization=0.9".into(),
                "--enable-auto-tool-choice".into(),
                // Qwen3.6 uses the `qwen3_coder` parser (per vLLM
                // Recipes). Qwen3.5 uses `qwen3_xml`. The supervisor's
                // `default_tool_parser_for_args` picks the right one
                // automatically based on the model id; listing
                // `qwen3_coder` explicitly here documents the
                // expected parser for the default-model case.
                "--tool-call-parser=qwen3_coder".into(),
                // Caches the system-prompt + tool-catalogue prefix
                // across rounds of a multi-step turn, dropping a
                // 3-round web_search → web_fetch → synthesise from
                // ~25s to ~10s. Off by default in vLLM; mandatory
                // for execlaw's chat path.
                "--enable-prefix-caching".into(),
            ],
            fields: vec![vllm_model_field("QuantTrio/Qwen3.6-27B-AWQ", "Model")],
        },
        BackendPreset {
            id: "ollama-apple".into(),
            purpose: BackendPurpose::Standard.as_str().to_owned(),
            inference_backend: "service-ollama".into(),
            name: "Ollama (Apple Silicon)".into(),
            description: "Native llama.cpp via Ollama on Metal. Runs as a managed `ollama serve` subprocess on the macOS host — Docker Desktop on Mac has no Metal passthrough, so the wizard skips containers for Apple GPUs. Requires `brew install ollama`; the supervisor pulls the selected model via `/api/pull` before serving.".into(),
            image: "".into(), // native runtime — no image
            // Ollama's daemon listens on 11434 by default. The
            // supervisor remaps to host_port_for(Standard) via
            // OLLAMA_HOST when spawning.
            container_port: 11434,
            vendor: PresetVendor::Apple.as_str().to_owned(),
            default_args: vec!["serve".into()],
            fields: vec![ollama_model_field("qwen2.5:32b-instruct-q4_K_M")],
        },
        BackendPreset {
            id: "vllm-cpu".into(),
            purpose: BackendPurpose::Standard.as_str().to_owned(),
            inference_backend: "service-vllm".into(),
            name: "vLLM (CPU)".into(),
            // vLLM doesn't publish a CPU image; the deployment
            // builds one from Dockerfile.cpu and tags it under the
            // execlaw namespace. Operator override is encouraged
            // via the advanced disclosure.
            description: "CPU-only fallback for hosts without a supported GPU. Slow but functional for dev or smoke tests; image must be built locally from vLLM's Dockerfile.cpu.".into(),
            image: "execlaw/service-vllm-cpu:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Cpu.as_str().to_owned(),
            default_args: vec![
                "--enable-auto-tool-choice".into(),
                "--tool-call-parser=qwen3_coder".into(),
                "--enable-prefix-caching".into(),
            ],
            fields: vec![vllm_model_field("QuantTrio/Qwen3.6-27B-AWQ", "Model")],
        },
        // ---------- Small (fast-path LLM) ----------
        BackendPreset {
            id: "vllm-small-cuda".into(),
            purpose: BackendPurpose::Small.as_str().to_owned(),
            inference_backend: "service-vllm".into(),
            name: "vLLM Small (NVIDIA)".into(),
            description: "Same vLLM image as Standard; pinned to the small Qwen variant for voice-mode fast-path latency.".into(),
            image: "vllm/vllm-openai:v0.20.2".into(),
            container_port: 8000,
            vendor: PresetVendor::Nvidia.as_str().to_owned(),
            default_args: vec![
                "--gpu-memory-utilization=0.5".into(),
                "--enable-auto-tool-choice".into(),
                "--tool-call-parser=hermes".into(),
                "--enable-prefix-caching".into(),
            ],
            fields: vec![vllm_model_field("Qwen/Qwen2.5-3B-Instruct-AWQ", "Model")],
        },
        BackendPreset {
            id: "ollama-apple-small".into(),
            purpose: BackendPurpose::Small.as_str().to_owned(),
            inference_backend: "service-ollama".into(),
            name: "Ollama Small (Apple Silicon)".into(),
            description: "Same managed Ollama subprocess as the Standard Apple preset; pinned to a small Qwen quant for voice-mode fast-path latency on Apple Silicon.".into(),
            image: "".into(),
            container_port: 11434,
            vendor: PresetVendor::Apple.as_str().to_owned(),
            default_args: vec!["serve".into()],
            fields: vec![ollama_model_field("qwen2.5:3b-instruct-q4_K_M")],
        },
        BackendPreset {
            id: "vllm-small-cpu".into(),
            purpose: BackendPurpose::Small.as_str().to_owned(),
            inference_backend: "service-vllm".into(),
            name: "vLLM Small (CPU)".into(),
            description: "CPU fallback for the fast-path slot. Acceptable for low-volume dev work; image must be built locally from vLLM's Dockerfile.cpu.".into(),
            image: "execlaw/service-vllm-cpu:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Cpu.as_str().to_owned(),
            default_args: vec![
                "--enable-auto-tool-choice".into(),
                "--tool-call-parser=hermes".into(),
                "--enable-prefix-caching".into(),
            ],
            fields: vec![vllm_model_field("Qwen/Qwen2.5-3B-Instruct-AWQ", "Model")],
        },
        // ---------- VoiceSTT (Whisper) ----------
        BackendPreset {
            id: "whisper-faster-cuda".into(),
            purpose: BackendPurpose::VoiceStt.as_str().to_owned(),
            inference_backend: "service-whisper-stt".into(),
            name: "faster-whisper (NVIDIA)".into(),
            description: "CTranslate2-based faster-whisper on CUDA. Locked-decision STT for NVIDIA hosts.".into(),
            image: "execlaw/service-whisper-cuda:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Nvidia.as_str().to_owned(),
            default_args: vec![],
            fields: vec![whisper_model_size_field()],
        },
        BackendPreset {
            id: "whisper-openvino-arc".into(),
            purpose: BackendPurpose::VoiceStt.as_str().to_owned(),
            inference_backend: "service-whisper-stt".into(),
            name: "Whisper OpenVINO (Intel Arc)".into(),
            description: "OpenVINO GenAI WhisperPipeline tuned for Intel Arc. Locked-decision STT for Intel hosts.".into(),
            image: "execlaw/service-whisper-openvino:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Intel.as_str().to_owned(),
            default_args: vec![],
            fields: vec![whisper_model_size_field()],
        },
        BackendPreset {
            id: "whisper-cpu".into(),
            purpose: BackendPurpose::VoiceStt.as_str().to_owned(),
            inference_backend: "service-whisper-stt".into(),
            name: "Whisper (CPU)".into(),
            description: "CPU-only Whisper. Latency depends on model size; tiny / base are usually acceptable on a modern CPU.".into(),
            image: "execlaw/service-whisper-cpu:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Cpu.as_str().to_owned(),
            default_args: vec![],
            fields: vec![whisper_model_size_field()],
        },
        // ---------- VoiceTTS (Kokoro / Piper) ----------
        BackendPreset {
            id: "kokoro-cuda".into(),
            purpose: BackendPurpose::VoiceTts.as_str().to_owned(),
            inference_backend: "service-kokoro-tts".into(),
            name: "Kokoro-82M (NVIDIA)".into(),
            description: "Kokoro v1.0 ONNX runtime on CUDA. Voice id is per-conversation via Settings → Personality.".into(),
            image: "execlaw/service-kokoro-cuda:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Nvidia.as_str().to_owned(),
            default_args: vec![],
            fields: vec![],
        },
        BackendPreset {
            id: "kokoro-openvino-arc".into(),
            purpose: BackendPurpose::VoiceTts.as_str().to_owned(),
            inference_backend: "service-kokoro-tts".into(),
            name: "Kokoro OpenVINO (Intel Arc)".into(),
            description: "Kokoro on OpenVINO for Intel Arc. Same per-conversation voice id as the CUDA variant.".into(),
            image: "execlaw/service-kokoro-openvino:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Intel.as_str().to_owned(),
            default_args: vec![],
            fields: vec![],
        },
        BackendPreset {
            id: "piper-cpu".into(),
            purpose: BackendPurpose::VoiceTts.as_str().to_owned(),
            inference_backend: "service-piper-tts".into(),
            name: "Piper (CPU)".into(),
            description: "Piper as the CPU TTS fallback. Lower-quality voices than Kokoro but runs anywhere.".into(),
            image: "execlaw/service-piper:v1".into(),
            container_port: 8000,
            vendor: PresetVendor::Cpu.as_str().to_owned(),
            default_args: vec![],
            fields: vec![],
        },
    ]
}

/// Per-purpose presets, each annotated with `recommended: true` when
/// its vendor matches the detected GPU. CPU presets are recommended
/// only when no GPU was detected; on GPU-equipped hosts the GPU-
/// specific preset wins.
pub fn presets_for(purpose: BackendPurpose, detected_vendors: &[GpuVendor]) -> Vec<PresetWithFlag> {
    let purpose_str = purpose.as_str();
    // Only count vendors we actually ship presets for. AMD hosts
    // *would* show up in `detected_vendors` if a Radeon were
    // installed, but we don't ship ROCm presets in v1, so an
    // AMD-only host should fall back to the CPU preset rather than
    // sit with zero recommendations. The check below mirrors that
    // by only marking `has_supported_gpu` when an NVIDIA or Intel
    // GPU is present.
    let has_supported_gpu = detected_vendors
        .iter()
        .any(|v| matches!(v, GpuVendor::Nvidia | GpuVendor::Intel | GpuVendor::Apple));
    all_presets()
        .into_iter()
        .filter(|p| p.purpose == purpose_str)
        .map(|p| {
            let recommended = match p.vendor.as_str() {
                "nvidia" => detected_vendors.contains(&GpuVendor::Nvidia),
                "intel" => detected_vendors.contains(&GpuVendor::Intel),
                "apple" => detected_vendors.contains(&GpuVendor::Apple),
                "cpu" => !has_supported_gpu,
                _ => false,
            };
            PresetWithFlag {
                preset: p,
                recommended,
            }
        })
        .collect()
}

/// Materialise a preset into a `model_spec_json` value the operator
/// can save into `config_backends.model_spec_json`. `field_values`
/// maps `PresetField.kind` → operator selection (e.g. `"model_size" -> "small"`).
///
/// Unknown fields in `field_values` are ignored (forward-compatible
/// with future preset additions). Fields the preset declares but the
/// operator didn't supply use the field's `default`.
pub fn materialise_spec(
    preset: &BackendPreset,
    field_values: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut args: Vec<String> = preset.default_args.clone();
    // Track every field's resolved value so the native-runtime
    // envelope can surface non-arg fields (model name for Ollama)
    // without re-walking the preset.
    let mut resolved: std::collections::HashMap<&str, String> =
        std::collections::HashMap::new();
    for field in &preset.fields {
        let value = field_values
            .get(&field.kind)
            .cloned()
            .unwrap_or_else(|| field.default.clone());
        resolved.insert(field.kind.as_str(), value.clone());
        if !field.arg_template.is_empty() {
            args.push(field.arg_template.replace("{value}", &value));
        }
    }

    // Apple-vendor presets run as native subprocesses (no Metal
    // passthrough into Linux containers exists). Emit a different
    // envelope so the supervisor's runtime-dispatch step picks the
    // NativeServiceController instead of bollard. `binary_hint` is
    // derived from the inference-backend plugin id (`service-ollama`
    // → "ollama") so future engines (`service-mlx`, `service-llama-cpp`)
    // slot in without touching the preset library.
    if preset.vendor == PresetVendor::Apple.as_str() {
        let binary_hint = preset
            .inference_backend
            .strip_prefix("service-")
            .unwrap_or(&preset.inference_backend)
            .to_owned();
        return serde_json::json!({
            "runtime": "native",
            "binary_hint": binary_hint,
            "args": args,
            "container_port": preset.container_port,
            "model": resolved.get("model").cloned().unwrap_or_default(),
        });
    }

    serde_json::json!({
        "image": preset.image,
        "args": args,
        "container_port": preset.container_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn preset_library_covers_every_purpose() {
        for purpose in BackendPurpose::all() {
            let presets: Vec<_> = all_presets()
                .into_iter()
                .filter(|p| p.purpose == purpose.as_str())
                .collect();
            assert!(
                !presets.is_empty(),
                "purpose {} has no presets — operators of unrecognised hardware would have no managed-mode option",
                purpose.as_str()
            );
        }
    }

    #[test]
    fn every_purpose_has_a_cpu_fallback() {
        // No-GPU hosts must still be able to pick managed mode for
        // every purpose. We don't ship every purpose with every
        // vendor (no Intel for vLLM yet), but CPU is always the safe
        // fallback.
        for purpose in BackendPurpose::all() {
            let has_cpu = all_presets()
                .into_iter()
                .any(|p| p.purpose == purpose.as_str() && p.vendor == "cpu");
            assert!(
                has_cpu,
                "purpose {} is missing a CPU fallback preset",
                purpose.as_str()
            );
        }
    }

    #[test]
    fn presets_for_recommends_nvidia_when_detected() {
        let presets = presets_for(BackendPurpose::VoiceStt, &[GpuVendor::Nvidia]);
        let cuda = presets
            .iter()
            .find(|p| p.preset.id == "whisper-faster-cuda")
            .expect("nvidia preset present");
        assert!(cuda.recommended);
        let intel = presets
            .iter()
            .find(|p| p.preset.id == "whisper-openvino-arc")
            .expect("intel preset present");
        assert!(!intel.recommended);
        let cpu = presets
            .iter()
            .find(|p| p.preset.id == "whisper-cpu")
            .expect("cpu preset present");
        assert!(
            !cpu.recommended,
            "CPU should NOT be recommended when an NVIDIA GPU is detected"
        );
    }

    #[test]
    fn presets_for_recommends_cpu_when_no_gpu_detected() {
        let presets = presets_for(BackendPurpose::VoiceStt, &[]);
        let cpu = presets
            .iter()
            .find(|p| p.preset.id == "whisper-cpu")
            .unwrap();
        assert!(cpu.recommended);
        // Both GPU presets remain available but unrecommended.
        for p in presets.iter() {
            if p.preset.vendor != "cpu" {
                assert!(!p.recommended);
            }
        }
    }

    #[test]
    fn presets_for_recommends_cpu_on_amd_only_host() {
        // We don't ship ROCm presets in v1, so an AMD-only host
        // would otherwise sit with zero recommendations and the
        // wizard's auto-pick would land on an NVIDIA card. Treat
        // AMD as "no supported GPU" so the CPU preset gets the
        // recommended highlight instead.
        let presets = presets_for(BackendPurpose::VoiceStt, &[GpuVendor::Amd]);
        let cpu = presets
            .iter()
            .find(|p| p.preset.id == "whisper-cpu")
            .expect("cpu preset present");
        assert!(
            cpu.recommended,
            "AMD-only host must recommend the CPU preset until ROCm presets ship"
        );
        // Neither GPU-specific preset should be recommended.
        for p in presets.iter() {
            if p.preset.vendor != "cpu" {
                assert!(!p.recommended);
            }
        }
    }

    #[test]
    fn presets_for_handles_dual_vendor_hosts() {
        // The locked-decision dev rig has both NVIDIA + Intel Arc.
        // The wizard should mark BOTH GPU presets as recommended so
        // the operator picks intentionally; CPU stays unrecommended.
        let presets = presets_for(
            BackendPurpose::VoiceStt,
            &[GpuVendor::Nvidia, GpuVendor::Intel],
        );
        assert!(
            presets
                .iter()
                .find(|p| p.preset.id == "whisper-faster-cuda")
                .unwrap()
                .recommended
        );
        assert!(
            presets
                .iter()
                .find(|p| p.preset.id == "whisper-openvino-arc")
                .unwrap()
                .recommended
        );
        assert!(
            !presets
                .iter()
                .find(|p| p.preset.id == "whisper-cpu")
                .unwrap()
                .recommended
        );
    }

    #[test]
    fn whisper_preset_exposes_model_size_with_small_default() {
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "whisper-faster-cuda")
            .unwrap();
        let field = preset
            .fields
            .iter()
            .find(|f| f.kind == "model_size")
            .expect("Whisper preset must expose model_size");
        assert_eq!(field.default, "small");
        assert!(field.choices.contains(&"tiny".to_owned()));
        assert!(field.choices.contains(&"large-v3".to_owned()));
    }

    #[test]
    fn kokoro_preset_has_no_configurable_fields() {
        // voice_id comes from Settings → Personality at request
        // time; there's no per-backend knob.
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "kokoro-cuda")
            .unwrap();
        assert!(preset.fields.is_empty());
    }

    #[test]
    fn materialise_substitutes_field_values_into_args() {
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "whisper-faster-cuda")
            .unwrap();
        let mut values = HashMap::new();
        values.insert("model_size".into(), "medium".into());
        let spec = materialise_spec(&preset, &values);
        assert_eq!(spec["image"], "execlaw/service-whisper-cuda:v1");
        assert_eq!(spec["container_port"], 8000);
        let args = spec["args"].as_array().unwrap();
        assert!(args.iter().any(|v| v == "--model-size=medium"));
    }

    #[test]
    fn materialise_uses_field_default_when_value_missing() {
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "whisper-faster-cuda")
            .unwrap();
        let values = HashMap::new();
        let spec = materialise_spec(&preset, &values);
        let args = spec["args"].as_array().unwrap();
        assert!(
            args.iter().any(|v| v == "--model-size=small"),
            "missing field must fall through to default ('small')"
        );
    }

    #[test]
    fn materialise_ignores_unknown_field_kinds() {
        // Forward-compat: a future SPA sends a knob the server
        // doesn't know about. The materialiser ignores it instead
        // of erroring, so an old server doesn't break a newer SPA.
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "kokoro-cuda")
            .unwrap();
        let mut values = HashMap::new();
        values.insert("future_knob".into(), "value".into());
        let spec = materialise_spec(&preset, &values);
        assert_eq!(spec["image"], "execlaw/service-kokoro-cuda:v1");
        // No extra args appeared.
        let args = spec["args"].as_array().unwrap();
        assert!(args.is_empty());
    }

    #[test]
    fn preset_ids_are_unique_globally() {
        // Stable ids are the SPA's reference key. A dup would cause
        // the wizard to silently materialise the wrong preset.
        let mut seen = std::collections::HashSet::new();
        for p in all_presets() {
            assert!(seen.insert(p.id.clone()), "duplicate preset id: {}", p.id);
        }
    }

    // ---- Apple Silicon presets --------------------------------------

    #[test]
    fn presets_for_recommends_apple_ollama_when_apple_detected() {
        let presets = presets_for(BackendPurpose::Standard, &[GpuVendor::Apple]);
        let apple = presets
            .iter()
            .find(|p| p.preset.id == "ollama-apple")
            .expect("apple Standard preset must exist");
        assert!(apple.recommended, "Apple GPU should recommend ollama-apple");
        let cpu = presets
            .iter()
            .find(|p| p.preset.id == "vllm-cpu")
            .expect("CPU fallback present");
        assert!(
            !cpu.recommended,
            "CPU preset must NOT be recommended when an Apple GPU is detected"
        );
        let nvidia = presets
            .iter()
            .find(|p| p.preset.id == "vllm-cuda")
            .expect("NVIDIA preset present");
        assert!(
            !nvidia.recommended,
            "NVIDIA preset must NOT be recommended on a non-NVIDIA host"
        );
    }

    #[test]
    fn presets_for_recommends_apple_ollama_small_when_apple_detected() {
        // Mirrors the Standard test for the Small purpose — confirms
        // both Apple presets surface as `recommended` on a Mac host.
        let presets = presets_for(BackendPurpose::Small, &[GpuVendor::Apple]);
        let apple = presets
            .iter()
            .find(|p| p.preset.id == "ollama-apple-small")
            .expect("apple Small preset must exist");
        assert!(apple.recommended);
    }

    #[test]
    fn apple_preset_materialises_native_runtime_envelope() {
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "ollama-apple")
            .unwrap();
        let mut values = HashMap::new();
        values.insert("model".into(), "qwen2.5:14b-instruct-q4_K_M".into());
        let spec = materialise_spec(&preset, &values);
        assert_eq!(spec["runtime"], "native");
        assert_eq!(spec["binary_hint"], "ollama");
        assert_eq!(spec["model"], "qwen2.5:14b-instruct-q4_K_M");
        assert_eq!(spec["container_port"], 11434);
        // No `image` key in the native envelope — the SPA's advanced
        // disclosure should branch on the runtime field, not look for
        // an empty image to know "this is a native preset."
        assert!(
            spec.get("image").is_none(),
            "native-runtime envelope must NOT include `image`; got {spec}"
        );
        // `args` carries the daemon subcommand even though there's no
        // model arg — Ollama needs `serve`.
        let args = spec["args"].as_array().unwrap();
        assert_eq!(args, &vec![serde_json::Value::String("serve".into())]);
    }

    #[test]
    fn apple_preset_uses_default_model_when_no_value_supplied() {
        // Same forward-compat behaviour as other presets — a wizard
        // that doesn't include the model field should still produce
        // a materialised spec with the default.
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "ollama-apple")
            .unwrap();
        let spec = materialise_spec(&preset, &HashMap::new());
        assert_eq!(spec["model"], "qwen2.5:32b-instruct-q4_K_M");
    }

    #[test]
    fn non_apple_preset_still_emits_docker_envelope() {
        // Regression guard: the runtime dispatch branches on the
        // PresetVendor::Apple discriminator only — every other
        // preset must keep the existing Docker shape.
        let preset = all_presets()
            .into_iter()
            .find(|p| p.id == "vllm-cuda")
            .unwrap();
        let spec = materialise_spec(&preset, &HashMap::new());
        assert!(spec.get("runtime").is_none());
        assert!(spec.get("binary_hint").is_none());
        assert_eq!(spec["image"], "vllm/vllm-openai:v0.20.2");
    }

    #[test]
    fn presets_for_recommends_cpu_when_no_gpu_even_with_apple_presets() {
        // No-GPU host should fall back to CPU; the new Apple presets
        // must NOT win in this case (we don't want a Linux host with
        // no GPU to suddenly recommend Ollama-Apple).
        let presets = presets_for(BackendPurpose::Standard, &[]);
        let apple = presets.iter().find(|p| p.preset.id == "ollama-apple").unwrap();
        let cpu = presets.iter().find(|p| p.preset.id == "vllm-cpu").unwrap();
        assert!(!apple.recommended);
        assert!(cpu.recommended);
    }

    #[test]
    fn apple_presets_exist_for_standard_and_small_purposes() {
        // Sanity: don't ship one without the other — the supervisor
        // pairs Standard + Small for voice-mode fast-path.
        let ids: Vec<String> = all_presets().into_iter().map(|p| p.id).collect();
        assert!(ids.iter().any(|s| s == "ollama-apple"));
        assert!(ids.iter().any(|s| s == "ollama-apple-small"));
    }
}
