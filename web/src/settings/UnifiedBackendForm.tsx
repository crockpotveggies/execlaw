// Phase 14 follow-up — shared backend-picker form.
//
// Single source of truth for the wizard UX both Settings → Backends
// (inline panel) and the first-run /setup wizard render. Replaces
// the legacy BackendWizardPanel (preset-card list) so both surfaces
// list Intel Arc + NVIDIA GPU targets identically and share the
// same memory-filtered model catalog.
//
// Shape:
//
//   * One <select> for the "target" — every detected GPU plus a
//     "Remote OpenAI-compatible endpoint" sentinel.
//   * Conditional serving-method radios (NVIDIA → vLLM only; Intel
//     Arc → OpenVINO vs OpenArc).
//   * A model dropdown filtered to entries that fit in the picked
//     GPU's VRAM. Falls back to the full catalog when memory_mb is
//     unknown.
//   * Remote target → URL + optional model-id form.
//
// Submits via the parent-supplied `onSubmit` so the same component
// can write through different upsert paths (the wizard saves
// directly via upsertBackend; the BackendsPage may want to defer
// the save into its existing edit form).

import {
    useCallback,
    useEffect,
    useState,
    type FormEvent,
} from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import { ErrorBanner } from "../components/ErrorBanner";
import {
    type BackendPurpose,
    type DetectedGpu,
    type UpsertBackendRequest,
} from "../api/endpoints";

export type ServingMethod = "vllm" | "openvino" | "openarc" | "ollama";

interface ModelOption {
    /// Hugging Face repo id / OpenVINO model id — written into
    /// `--model={id}` for the chosen serving image.
    id: string;
    /// Display label in the dropdown.
    label: string;
    /// Approximate minimum VRAM required, in MiB. The form hides
    /// entries that exceed the picked GPU's `memory_mb`.
    min_mb: number;
    /// Approximate on-disk size of the model weights, in MiB. Used
    /// for the disk-space preflight check — the wizard refuses to
    /// download a model that won't fit (free space - 5 GB safety
    /// margin). Defaults to `min_mb` when omitted, since AWQ /
    /// INT4 model files are roughly the same size as VRAM usage.
    disk_mb?: number;
}

const SERVING_LABEL: Record<ServingMethod, string> = {
    vllm: "vLLM",
    openvino: "OpenVINO model server",
    openarc: "OpenArc",
    ollama: "Ollama (Metal, native)",
};

const SERVING_PLUGIN: Record<ServingMethod, string> = {
    vllm: "service-vllm",
    openvino: "service-vllm-openvino-arc",
    openarc: "service-openarc",
    ollama: "service-ollama",
};

// Image references for the wizard's serving methods. We track the
// nightly vLLM tag because it's the only stream that ships Qwen 3.5
// (and other recent architecture) support without waiting on a
// stable cut. The pinned `v0.6.2` we used before pre-dated Qwen 3.x
// — every spawn against it crashed with "unknown architecture" and
// the operator had to dig through container logs to figure out why.
//
// `nightly` is built daily from main and tested by the vLLM team
// before publishing. If you need a deterministic build for prod,
// override the image via the model_spec JSON in Settings → Backends
// after setup.
const SERVING_IMAGE: Record<ServingMethod, string> = {
    vllm: "vllm/vllm-openai:nightly",
    openvino: "execlaw/service-vllm-openvino-arc:v1",
    openarc: "execlaw/service-openarc:v1",
    // Native runtime — no container image. The supervisor reads
    // `runtime: "native"` + `binary_hint: "ollama"` from model_spec
    // and spawns via NativeServiceController instead of bollard.
    // Field exists only because every other ServingMethod populates
    // it; the wizard's Apple save path emits an empty `image`.
    ollama: "",
};

// Curated model catalog. Every entry MUST be a real, downloadable
// HuggingFace repo id — vLLM resolves `--model={id}` against the HF
// registry on first start, and a 404 here means the container exits
// with a non-zero code on every spawn (CrashLooping with no obvious
// cause).
//
// The flagship is the locked-decision Qwen 3.5 27B AWQ. Pairs with
// the `nightly` vLLM image because Qwen 3.5 architecture support
// hasn't reached a stable cut yet. Smaller fallbacks are Qwen 2.5
// AWQ which the vLLM nightly also supports — operators on
// 12 GB / 8 GB / 4 GB cards pick down the list. Operators with a
// private model override the id post-setup via Settings → Backends
// → raw JSON.
const MODEL_CATALOG: Record<ServingMethod, ModelOption[]> = {
    vllm: [
        {
            id: "QuantTrio/Qwen3.5-27B-AWQ",
            label: "Qwen 3.5 27B (AWQ, ~18 GB) — flagship",
            min_mb: 18_000,
        },
        {
            id: "Qwen/Qwen2.5-32B-Instruct-AWQ",
            label: "Qwen 2.5 32B Instruct (AWQ, ~20 GB)",
            min_mb: 20_000,
        },
        {
            id: "Qwen/Qwen2.5-14B-Instruct-AWQ",
            label: "Qwen 2.5 14B Instruct (AWQ, ~10 GB)",
            min_mb: 10_000,
        },
        {
            id: "Qwen/Qwen2.5-7B-Instruct-AWQ",
            label: "Qwen 2.5 7B Instruct (AWQ, ~6 GB)",
            min_mb: 6_000,
        },
        {
            id: "Qwen/Qwen2.5-3B-Instruct-AWQ",
            label: "Qwen 2.5 3B Instruct (AWQ, ~3 GB)",
            min_mb: 3_000,
        },
    ],
    openvino: [
        {
            id: "OpenVINO/Qwen2.5-7B-Instruct-int4-ov",
            label: "Qwen 2.5 7B (INT4 OpenVINO, ~6 GB)",
            min_mb: 6_000,
        },
        {
            id: "OpenVINO/Phi-3-mini-4k-instruct-int4-ov",
            label: "Phi-3 Mini 4k (INT4 OpenVINO, ~3 GB)",
            min_mb: 3_000,
        },
    ],
    openarc: [
        {
            id: "OpenVINO/Qwen2.5-7B-Instruct-int4-ov",
            label: "Qwen 2.5 7B (INT4, ~6 GB)",
            min_mb: 6_000,
        },
        {
            id: "OpenVINO/Phi-3-mini-4k-instruct-int4-ov",
            label: "Phi-3 Mini 4k (INT4, ~3 GB)",
            min_mb: 3_000,
        },
    ],
    // Apple Silicon — Ollama registry tags. Qwen3.5 isn't published
    // to the Ollama registry yet (as of 2026-05); we pin Qwen2.5 in
    // the four parameter sizes that match the preset library's
    // ollama_model_field choices. Operators with a private model
    // override post-setup via the raw model_spec_json. The on-disk
    // size matches the unified-memory budget closely on Apple
    // Silicon because the GPU shares RAM with the CPU — the wizard's
    // disk-space check uses these as a proxy.
    ollama: [
        {
            id: "qwen2.5:32b-instruct-q4_K_M",
            label: "Qwen 2.5 32B Instruct (q4_K_M, ~20 GB)",
            min_mb: 20_000,
        },
        {
            id: "qwen2.5:14b-instruct-q4_K_M",
            label: "Qwen 2.5 14B Instruct (q4_K_M, ~9 GB)",
            min_mb: 9_000,
        },
        {
            id: "qwen2.5:7b-instruct-q4_K_M",
            label: "Qwen 2.5 7B Instruct (q4_K_M, ~5 GB)",
            min_mb: 5_000,
        },
        {
            id: "qwen2.5:3b-instruct-q4_K_M",
            label: "Qwen 2.5 3B Instruct (q4_K_M, ~2 GB)",
            min_mb: 2_000,
        },
    ],
};

interface ManagedTarget {
    kind: "gpu";
    gpu: DetectedGpu;
    gpuIdx: number;
}

interface RemoteTarget {
    kind: "remote";
}

type Target = ManagedTarget | RemoteTarget;

const REMOTE_TARGET_KEY = "__remote";

function targetKey(t: Target): string {
    if (t.kind === "remote") return REMOTE_TARGET_KEY;
    return `gpu:${t.gpuIdx}`;
}

function targetLabel(t: Target): string {
    if (t.kind === "remote") return "Remote OpenAI-compatible endpoint";
    return gpuLabel(t.gpu);
}

function servingMethodsFor(gpu: DetectedGpu): ServingMethod[] {
    switch (gpu.vendor) {
        case "Nvidia":
            return ["vllm"];
        case "Intel":
            return ["openvino", "openarc"];
        case "Apple":
            // Native Ollama subprocess — no Docker. The form's Apple
            // branch renders the install-prompt panel when
            // ollamaAvailable === false, otherwise the standard
            // model picker.
            return ["ollama"];
        default:
            return [];
    }
}

function modelsFor(serving: ServingMethod, memory_mb: number | null): ModelOption[] {
    const all = MODEL_CATALOG[serving];
    if (memory_mb === null) return all;
    return all.filter((m) => m.min_mb <= memory_mb);
}

export interface UnifiedBackendFormProps {
    /// The slot being configured. Drives the submit purpose; not
    /// shown in the form itself.
    purpose: BackendPurpose;
    gpus: DetectedGpu[];
    /// Whether the host has a reachable Docker daemon. When false,
    /// Docker-dependent GPU targets (NVIDIA, Intel) are hidden and
    /// only Remote (+ any non-Docker target like Apple/Ollama) is
    /// offered.
    dockerAvailable: boolean;
    /// Whether Ollama is installed + runnable on the host. Apple
    /// GPUs use it as their native runtime (no Metal-to-container
    /// passthrough exists on macOS). When `false`, the form still
    /// surfaces Apple GPUs as a target but renders an install panel
    /// with `brew install ollama` instructions and disables Save
    /// until the operator clicks Recheck and we re-probe `available`.
    /// Optional — wizard callers added pre-2026-05-11 can omit it
    /// (default `false`); Apple targets then always render the
    /// install panel.
    ollamaAvailable?: boolean;
    /// Trimmed `ollama --version` output, e.g. `"0.1.43"`. Rendered
    /// in the "Ollama X.Y.Z detected" confirmation badge above the
    /// model picker. Optional; omitted/null hides the version line.
    ollamaVersion?: string | null;
    /// Absolute path the discoverer landed on (Homebrew prefix or
    /// PATH). Shown next to the version for disambiguation on
    /// multi-install systems.
    ollamaPath?: string | null;
    /// Re-probe trigger. Wizard callers should call the preflight
    /// endpoint and update `ollamaAvailable/Version/Path` props.
    /// Optional — when omitted the form hides the Recheck button.
    onRecheckOllama?: () => Promise<void> | void;
    /// Free bytes on the host's HF cache volume (from the preflight
    /// response). When supplied + the picked model exceeds
    /// `disk_free_bytes - 5 GB safety margin`, the form renders a
    /// warning that running this model may fail. `null` means the
    /// probe couldn't determine free space; we render a softer
    /// "couldn't detect" warning instead.
    diskFreeBytes?: number | null;
    diskFreePath?: string | null;
    /// `model_id → bytes already cached on disk` from the preflight
    /// endpoint. When the picked model has an entry here, the form
    /// subtracts those bytes from the required-disk-space calculation
    /// (the weights don't need to be re-downloaded). Empty `{}` is
    /// fine — the disk check then assumes a fresh download. Defaults
    /// to `{}` for callers that haven't wired the new prop yet.
    cachedModels?: Record<string, number>;
    /// Called with the fully-formed UpsertBackendRequest. The parent
    /// owns the actual PUT so the form is reusable across both the
    /// first-run wizard's direct-save path and the Settings page's
    /// edit-then-confirm path.
    onSubmit: (purpose: BackendPurpose, body: UpsertBackendRequest) => Promise<void>;
    /// Optional skip affordance — wizard's "I'll do this later"
    /// button. Settings page can hide this by omitting the prop.
    onSkip?: () => void;
    /// Submit-button label. Defaults to "Save backend".
    submitLabel?: string;
    /// Skip-button label. Defaults to "Skip for now".
    skipLabel?: string;
    /// Test-id prefix so the wizard + settings versions can coexist
    /// in tests without colliding selectors.
    testIdPrefix?: string;
}

/// Safety margin we leave free above the picked model's on-disk
/// size. Covers vLLM's image cache writes, log rotations, and
/// general "you don't want a backend deploy to fill the disk to
/// 100%" headroom. Mirrors the supervisor's primary cache layout.
const DISK_SAFETY_MARGIN_MB = 5 * 1024;

/// Format an approximate byte count as "12.4 GB" / "850 MB".
function fmtMb(mb: number): string {
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    return `${Math.round(mb)} MB`;
}

function fmtBytes(b: number): string {
    if (b >= 1024 ** 3) return `${(b / 1024 ** 3).toFixed(1)} GB`;
    if (b >= 1024 ** 2) return `${(b / 1024 ** 2).toFixed(1)} MB`;
    return `${b} B`;
}

/// Apple-Silicon preflight panel — replaces the wizard's model-picker
/// step with an install-prompt banner when Ollama isn't present, and
/// with a small "detected" badge once it is. The form gates Save on
/// the same `available` flag so the operator can't submit a backend
/// row the supervisor will reject at spawn time.
///
/// Mirrors the Docker-Desktop install nudge the wizard already shows
/// when `dockerAvailable === false` and the operator picks an
/// NVIDIA/Intel target — but with Apple-specific copy.
function ApplePreflightPanel({
    available,
    version,
    path,
    onRecheck,
    submitting,
    testIdPrefix,
}: {
    available: boolean;
    version: string | null;
    path: string | null;
    onRecheck?: () => Promise<void> | void;
    submitting: boolean;
    testIdPrefix: string;
}) {
    const [rechecking, setRechecking] = useState(false);
    const handleRecheck = useCallback(async () => {
        if (!onRecheck) return;
        setRechecking(true);
        try {
            await onRecheck();
        } finally {
            setRechecking(false);
        }
    }, [onRecheck]);

    if (available) {
        const versionText = version ? ` v${version}` : "";
        const pathText = path ? ` · ${path}` : "";
        return (
            <div
                className="execlaw-muted small mb-3"
                data-testid={`${testIdPrefix}-ollama-detected`}
                data-ollama-state="available"
            >
                <i className="bi bi-check-circle-fill me-1" aria-hidden />
                Ollama{versionText} detected{pathText}
            </div>
        );
    }

    return (
        <div
            className="execlaw-card mb-3"
            data-testid={`${testIdPrefix}-ollama-install`}
            data-ollama-state="missing"
        >
            <div className="execlaw-card__title">
                <i
                    className="bi bi-exclamation-triangle-fill me-2"
                    aria-hidden
                />
                Install Ollama to enable Apple-Silicon inference
            </div>
            <p className="execlaw-muted small mb-2">
                Apple Silicon has no Metal-to-container passthrough, so
                execlaw spawns Ollama as a native macOS subprocess
                instead of a Docker container. Install it once and
                we&rsquo;ll detect it on Recheck:
            </p>
            <pre
                className="execlaw-code mb-2"
                style={{
                    padding: "0.5rem 0.75rem",
                    background: "rgba(255,255,255,0.04)",
                    borderRadius: "0.25rem",
                    fontSize: "0.85rem",
                    overflowX: "auto",
                }}
            >
                <code>brew install ollama</code>
            </pre>
            {path && (
                <p className="execlaw-muted small mb-2">
                    A binary was found at <code>{path}</code> but
                    <code> --version</code> failed — the install may be
                    corrupted. Run <code>{path} --version</code> in a
                    terminal to see the underlying error.
                </p>
            )}
            <p className="execlaw-muted small mb-2">
                Already installed somewhere else?{" "}
                Set <code>OLLAMA_BINARY</code> to the absolute path and
                restart execlaw.
            </p>
            {onRecheck && (
                <Button
                    type="button"
                    variant="outline-secondary"
                    size="sm"
                    disabled={submitting || rechecking}
                    onClick={() => void handleRecheck()}
                    data-testid={`${testIdPrefix}-ollama-recheck`}
                >
                    {rechecking ? (
                        <>
                            <Spinner
                                size="sm"
                                animation="border"
                                className="me-2"
                            />
                            Checking…
                        </>
                    ) : (
                        "Recheck"
                    )}
                </Button>
            )}
        </div>
    );
}

export function UnifiedBackendForm({
    purpose,
    gpus,
    dockerAvailable,
    ollamaAvailable = false,
    ollamaVersion = null,
    ollamaPath = null,
    onRecheckOllama,
    diskFreeBytes,
    diskFreePath,
    cachedModels = {},
    onSubmit,
    onSkip,
    submitLabel = "Save backend",
    skipLabel = "Skip for now",
    testIdPrefix = "unified-backend",
}: UnifiedBackendFormProps) {
    const targets: Target[] = [];
    // Apple GPUs go through the native Ollama runtime — no Docker
    // required. Docker-dependent GPUs (NVIDIA, Intel) stay gated on
    // dockerAvailable so a Mac-without-Docker host still sees its
    // Apple GPU as a target option (and the install-prompt panel
    // tells the operator how to make it work).
    gpus.forEach((g, idx) => {
        if (servingMethodsFor(g).length === 0) return;
        const requiresDocker = g.vendor !== "Apple";
        if (requiresDocker && !dockerAvailable) return;
        targets.push({ kind: "gpu", gpu: g, gpuIdx: idx });
    });
    targets.push({ kind: "remote" });

    const [targetIdx, setTargetIdx] = useState(0);
    const target = targets[targetIdx] ?? targets[0];

    const availableServing =
        target.kind === "gpu" ? servingMethodsFor(target.gpu) : [];
    const [serving, setServing] = useState<ServingMethod>(
        availableServing[0] ?? "vllm",
    );
    useEffect(() => {
        if (target.kind === "gpu") {
            const opts = servingMethodsFor(target.gpu);
            if (!opts.includes(serving)) setServing(opts[0]);
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [targetIdx]);

    const availableModels =
        target.kind === "gpu"
            ? modelsFor(serving, target.gpu.memory_mb ?? null)
            : [];
    const [modelId, setModelId] = useState<string>("");
    useEffect(() => {
        if (target.kind === "gpu") {
            if (
                modelId.length === 0 ||
                !availableModels.find((m) => m.id === modelId)
            ) {
                setModelId(availableModels[0]?.id ?? "");
            }
        } else {
            setModelId("");
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [targetIdx, serving, availableModels.length]);

    const [endpoint, setEndpoint] = useState("");
    const [remoteModel, setRemoteModel] = useState("");

    const [submitting, setSubmitting] = useState(false);
    const [submitError, setSubmitError] = useState<string | null>(null);
    const [endpointError, setEndpointError] = useState<string | null>(null);

    const submit = useCallback(
        async (e: FormEvent<HTMLFormElement>) => {
            e.preventDefault();
            setSubmitError(null);
            setEndpointError(null);
            setSubmitting(true);
            try {
                if (target.kind === "remote") {
                    const trimmed = endpoint.trim();
                    if (trimmed.length === 0) {
                        setEndpointError("Required.");
                        setSubmitting(false);
                        return;
                    }
                    try {
                        new URL(trimmed);
                    } catch {
                        setEndpointError(
                            "Doesn't look like a URL. Example: http://localhost:8000/v1",
                        );
                        setSubmitting(false);
                        return;
                    }
                    await onSubmit(purpose, {
                        inference_backend: "external",
                        model_spec:
                            remoteModel.trim().length > 0
                                ? { model: remoteModel.trim() }
                                : {},
                        gpu_id: null,
                        endpoint: trimmed,
                        notes: "Configured via backend wizard (remote)",
                        reasoning_enabled: false,
                        mode: "external",
                    });
                } else {
                    if (!modelId) {
                        setSubmitError(
                            "Pick a model that fits this GPU (or skip and configure later).",
                        );
                        setSubmitting(false);
                        return;
                    }
                    // Apple / native-Ollama path emits a different
                    // model_spec_json envelope (no `image`, carries
                    // `runtime` / `binary_hint` / `model`) so the
                    // supervisor's spec_from_row routes it through
                    // NativeServiceController. Mirrors what
                    // backend_presets::materialise_spec emits for
                    // the `ollama-apple` preset row.
                    if (serving === "ollama") {
                        await onSubmit(purpose, {
                            inference_backend: SERVING_PLUGIN[serving],
                            model_spec: {
                                runtime: "native",
                                binary_hint: "ollama",
                                args: ["serve"],
                                container_port: 11434,
                                model: modelId,
                            },
                            // No GPU id needed — Ollama discovers
                            // Metal devices on its own and a Mac
                            // never has more than one anyway.
                            gpu_id: null,
                            endpoint: null,
                            notes: `Configured via backend wizard (${SERVING_LABEL[serving]})`,
                            reasoning_enabled: false,
                            mode: "managed",
                        });
                    } else {
                        const vendorIdx = vendorOrdinal(gpus, target.gpuIdx);
                        const vendorTag = serverVendorTag(target.gpu.vendor);
                        await onSubmit(purpose, {
                            inference_backend: SERVING_PLUGIN[serving],
                            model_spec: {
                                image: SERVING_IMAGE[serving],
                                args: [`--model=${modelId}`],
                                container_port: 8000,
                                gpu_vendor: vendorTag,
                            },
                            gpu_id: vendorIdx,
                            endpoint: null,
                            notes: `Configured via backend wizard (${SERVING_LABEL[serving]})`,
                            reasoning_enabled: false,
                            mode: "managed",
                        });
                    }
                }
            } catch (err) {
                setSubmitError(err instanceof Error ? err.message : String(err));
            } finally {
                setSubmitting(false);
            }
        },
        [target, serving, modelId, endpoint, remoteModel, gpus, onSubmit, purpose],
    );

    return (
        <div data-testid={`${testIdPrefix}-form`}>
            <ErrorBanner
                message={submitError}
                onDismiss={() => setSubmitError(null)}
                className="mb-3"
                testId={`${testIdPrefix}-error`}
            />
            <Form noValidate onSubmit={submit}>
                <Form.Group className="mb-3">
                    <Form.Label className="execlaw-muted small mb-1">
                        Target
                    </Form.Label>
                    <Form.Select
                        value={targetIdx}
                        onChange={(e) =>
                            setTargetIdx(parseInt(e.target.value, 10))
                        }
                        disabled={submitting}
                        data-testid={`${testIdPrefix}-target-select`}
                    >
                        {targets.map((t, i) => (
                            <option key={targetKey(t)} value={i}>
                                {targetLabel(t)}
                            </option>
                        ))}
                    </Form.Select>
                    {target.kind === "remote" && (
                        <Form.Text className="execlaw-muted">
                            Point execlaw at any OpenAI-compatible
                            endpoint — vLLM, llama.cpp, Ollama, LM
                            Studio, or a hosted proxy. The control
                            plane only speaks{" "}
                            <code>POST /v1/chat/completions</code> (no
                            vendor SDKs).
                        </Form.Text>
                    )}
                </Form.Group>

                {target.kind === "gpu" && target.gpu.vendor === "Apple" && (
                    <ApplePreflightPanel
                        available={ollamaAvailable}
                        version={ollamaVersion}
                        path={ollamaPath}
                        onRecheck={onRecheckOllama}
                        submitting={submitting}
                        testIdPrefix={testIdPrefix}
                    />
                )}

                {target.kind === "gpu" && (
                    <>
                        {availableServing.length === 1 ? (
                            <div
                                className="execlaw-muted small mb-3"
                                data-testid={`${testIdPrefix}-serving-fixed`}
                            >
                                Serving method:{" "}
                                <strong>{SERVING_LABEL[serving]}</strong>{" "}
                                (only supported method for this GPU vendor today).
                            </div>
                        ) : (
                            <Form.Group
                                className="mb-3"
                                data-testid={`${testIdPrefix}-serving-picker`}
                            >
                                <Form.Label className="execlaw-muted small mb-1">
                                    Serving method
                                </Form.Label>
                                <div className="d-flex gap-3">
                                    {availableServing.map((m) => (
                                        <Form.Check
                                            key={m}
                                            type="radio"
                                            id={`${testIdPrefix}-serving-${m}`}
                                            name={`${testIdPrefix}-serving`}
                                            label={SERVING_LABEL[m]}
                                            checked={serving === m}
                                            onChange={() => setServing(m)}
                                            disabled={submitting}
                                            data-testid={`${testIdPrefix}-serving-${m}`}
                                        />
                                    ))}
                                </div>
                                <Form.Text className="execlaw-muted">
                                    OpenVINO is the standard Intel
                                    serving stack; OpenArc is a
                                    vLLM-compatible drop-in tuned for
                                    Arc.
                                </Form.Text>
                            </Form.Group>
                        )}

                        {/* Apple/Ollama: skip the model picker until
                            the install panel above reports the binary
                            is runnable. Avoids the operator picking a
                            model into a spec that the supervisor will
                            then reject at spawn time. */}
                        {target.kind === "gpu" &&
                        target.gpu.vendor === "Apple" &&
                        !ollamaAvailable ? null : (
                        <Form.Group className="mb-3">
                            <Form.Label className="execlaw-muted small mb-1">
                                Model
                            </Form.Label>
                            {availableModels.length === 0 ? (
                                <div
                                    className="execlaw-muted small"
                                    data-testid={`${testIdPrefix}-no-models`}
                                >
                                    No model in the curated catalog fits
                                    this GPU&rsquo;s {target.gpu.memory_mb ?? "?"}
                                    {" "}MiB of VRAM. Skip and configure a
                                    custom model later.
                                </div>
                            ) : (
                                <>
                                    <Form.Select
                                        value={modelId}
                                        onChange={(e) => setModelId(e.target.value)}
                                        disabled={submitting}
                                        data-testid={`${testIdPrefix}-model-select`}
                                    >
                                        {availableModels.map((m) => (
                                            <option key={m.id} value={m.id}>
                                                {m.label}
                                            </option>
                                        ))}
                                    </Form.Select>
                                    <Form.Text className="execlaw-muted">
                                        Filtered to entries that fit in
                                        this GPU&rsquo;s VRAM
                                        {target.gpu.memory_mb
                                            ? ` (${(target.gpu.memory_mb / 1024).toFixed(1)} GB)`
                                            : ""}
                                        .
                                    </Form.Text>
                                    <DiskSpaceWarning
                                        modelOption={availableModels.find(
                                            (m) => m.id === modelId,
                                        )}
                                        diskFreeBytes={diskFreeBytes}
                                        diskFreePath={diskFreePath}
                                        cachedBytes={cachedModels[modelId] ?? 0}
                                        testIdPrefix={testIdPrefix}
                                    />
                                </>
                            )}
                        </Form.Group>
                        )}
                    </>
                )}

                {target.kind === "remote" && (
                    <>
                        <Form.Group
                            className="mb-3"
                            controlId={`${testIdPrefix}-external-endpoint`}
                        >
                            <Form.Label>Endpoint URL</Form.Label>
                            <Form.Control
                                type="url"
                                value={endpoint}
                                onChange={(e) => setEndpoint(e.target.value)}
                                isInvalid={!!endpointError}
                                disabled={submitting}
                                placeholder="http://localhost:8000/v1"
                                data-testid={`${testIdPrefix}-external-endpoint`}
                            />
                            <Form.Control.Feedback type="invalid">
                                {endpointError}
                            </Form.Control.Feedback>
                            <Form.Text className="execlaw-muted">
                                Include the <code>/v1</code> suffix if
                                your server requires it.
                            </Form.Text>
                        </Form.Group>
                        <Form.Group
                            className="mb-3"
                            controlId={`${testIdPrefix}-external-model`}
                        >
                            <Form.Label>
                                Model id{" "}
                                <span className="execlaw-muted">(optional)</span>
                            </Form.Label>
                            <Form.Control
                                type="text"
                                value={remoteModel}
                                onChange={(e) => setRemoteModel(e.target.value)}
                                disabled={submitting}
                                placeholder="QuantTrio/Qwen3.5-27B-AWQ"
                                data-testid={`${testIdPrefix}-external-model`}
                            />
                            <Form.Text className="execlaw-muted">
                                Sent in the <code>model</code> field on
                                every request.
                            </Form.Text>
                        </Form.Group>
                    </>
                )}

                <div className="d-flex gap-2">
                    <Button
                        type="submit"
                        variant="primary"
                        // Apple/Ollama: don't let the operator submit
                        // until Ollama is actually present. The form
                        // would otherwise write a spec the supervisor
                        // can't spawn and the row would flip to
                        // CrashLooping after the wizard already
                        // claimed success — bad UX.
                        disabled={
                            submitting ||
                            (target.kind === "gpu" &&
                                target.gpu.vendor === "Apple" &&
                                !ollamaAvailable)
                        }
                        data-testid={`${testIdPrefix}-submit`}
                    >
                        {submitting ? (
                            <>
                                <Spinner
                                    size="sm"
                                    animation="border"
                                    className="me-2"
                                />
                                Saving…
                            </>
                        ) : (
                            submitLabel
                        )}
                    </Button>
                    {onSkip && (
                        <Button
                            type="button"
                            variant="outline-secondary"
                            onClick={onSkip}
                            disabled={submitting}
                            data-testid={`${testIdPrefix}-skip`}
                        >
                            {skipLabel}
                        </Button>
                    )}
                </div>
            </Form>
        </div>
    );
}

// ---------------------------------------------------------------------------
// Helpers — kept local so the SetupWizard's HardwareSummary can reuse
// them without depending on this file.
// ---------------------------------------------------------------------------

export function gpuLabel(g: DetectedGpu): string {
    const vendor = vendorDisplayName(g.vendor);
    const sku =
        g.model_name && g.model_name.trim().length > 0
            ? g.model_name.trim()
            : `${vendor} GPU (${cleanPciDeviceId(g.pci_device_id)})`;
    const mem =
        g.memory_mb && g.memory_mb > 0
            ? ` · ${(g.memory_mb / 1024).toFixed(1)} GB`
            : "";
    if (g.vendor === "Intel" && !sku.toLowerCase().startsWith("intel")) {
        return `Intel ${sku}${mem}`;
    }
    return `${sku}${mem}`;
}

function vendorDisplayName(v: DetectedGpu["vendor"]): string {
    switch (v) {
        case "Nvidia":
            return "NVIDIA";
        case "Intel":
            return "Intel";
        case "Amd":
            return "AMD";
        case "Apple":
            return "Apple Silicon (Metal)";
        default:
            return "GPU";
    }
}

function cleanPciDeviceId(raw: string): string {
    if (raw.startsWith("0x") || raw.startsWith("0X")) return raw;
    const devIdx = raw.indexOf("DEV_");
    if (devIdx >= 0) {
        const hex = raw.slice(devIdx + 4, devIdx + 8);
        if (/^[0-9a-fA-F]{4}$/.test(hex)) return `0x${hex.toLowerCase()}`;
    }
    return raw.length > 14 ? `${raw.slice(0, 13)}…` : raw;
}

export function gpuIdString(g: DetectedGpu): string {
    if (typeof g.id === "string") return g.id;
    if (g.id && typeof g.id === "object" && "0" in g.id) {
        return String(g.id[0]);
    }
    return `${g.pci_vendor_id}:${g.pci_device_id}`;
}

/// Per-vendor ordinal — matches nvidia-docker's `--gpus device=N`
/// semantics so the supervisor can pass it through to bollard
/// verbatim.
export function vendorOrdinal(gpus: DetectedGpu[], idx: number): string {
    const vendor = gpus[idx]?.vendor;
    let n = 0;
    for (let i = 0; i < idx; i++) {
        if (gpus[i].vendor === vendor) n++;
    }
    return String(n);
}

export function serverVendorTag(v: DetectedGpu["vendor"]): string | undefined {
    switch (v) {
        case "Nvidia":
            return "nvidia";
        case "Intel":
            return "intel";
        case "Amd":
            return "amd";
        case "Apple":
            return "apple";
        default:
            return undefined;
    }
}

/// Disk-space preflight warning. Renders one of three states:
///
///   * `null`-disk-free: "couldn't detect free space" — softer
///     yellow warning with a hint that downloads + inference may
///     not work.
///   * Insufficient space: hard red warning with a concrete
///     "you need X.X GB more" message + the detected path so the
///     operator knows which volume to free up.
///   * Sufficient: nothing rendered. Operator gets confidence
///     from the absence of a warning rather than a noisy "ok"
///     check.
function DiskSpaceWarning({
    modelOption,
    diskFreeBytes,
    diskFreePath,
    cachedBytes,
    testIdPrefix,
}: {
    modelOption: ModelOption | undefined;
    diskFreeBytes: number | null | undefined;
    diskFreePath: string | null | undefined;
    /// Bytes of the picked model already present in the HF cache.
    /// Subtracted from the model's disk requirement — the weights
    /// don't need to be re-acquired. `0` (default) means a fresh
    /// download is assumed.
    cachedBytes: number;
    testIdPrefix: string;
}) {
    if (!modelOption) return null;
    const totalMb = modelOption.disk_mb ?? modelOption.min_mb;
    const totalBytes = totalMb * 1024 * 1024;
    // How many additional bytes do we actually need to download?
    // Assumes the cached files cover the model — a partial download
    // could leave us short, but vLLM resumes correctly via the HF
    // cache lock so this is the conservative-enough estimate. Clamp
    // to 0 so a slightly-larger-on-disk-than-our-estimate cache hit
    // doesn't go negative.
    const additionalBytesNeeded = Math.max(0, totalBytes - cachedBytes);
    const safetyMarginBytes = DISK_SAFETY_MARGIN_MB * 1024 * 1024;
    const requiredBytes = additionalBytesNeeded + safetyMarginBytes;
    const requiredMb = Math.ceil(requiredBytes / (1024 * 1024));
    // Cached enough to cover the weights — render a positive
    // confirmation chip so the operator knows the warning was
    // correctly suppressed (rather than a probe failure).
    const isFullyCached = cachedBytes >= totalBytes;

    if (diskFreeBytes === undefined || diskFreeBytes === null) {
        // Probe failed — render a soft warning rather than block
        // the operator. The supervisor will surface a real error
        // at download time if disk truly runs out.
        return (
            <div
                className="execlaw-error-banner mt-2"
                style={{
                    backgroundColor: "rgba(210, 153, 34, 0.16)",
                    color: "#e9b94d",
                }}
                role="alert"
                data-testid={`${testIdPrefix}-disk-warning-unknown`}
            >
                <strong>Couldn&rsquo;t detect free disk space.</strong>{" "}
                Downloading the model ({fmtMb(totalMb)}) and running
                it may fail if there isn&rsquo;t enough room on the
                volume hosting the cache. Free up at least{" "}
                <strong>{fmtMb(requiredMb)}</strong> before continuing.
            </div>
        );
    }

    if (diskFreeBytes < requiredBytes) {
        const shortfallBytes = requiredBytes - diskFreeBytes;
        return (
            <div
                className="execlaw-error-banner mt-2"
                role="alert"
                data-testid={`${testIdPrefix}-disk-warning-insufficient`}
            >
                <strong>Not enough disk space.</strong>{" "}
                {cachedBytes > 0 ? (
                    <>
                        {fmtBytes(cachedBytes)} of this model is already
                        cached, but the remaining{" "}
                        <strong>{fmtBytes(additionalBytesNeeded)}</strong>{" "}
                    </>
                ) : (
                    <>
                        This model needs <strong>{fmtMb(totalMb)}</strong>{" "}
                    </>
                )}
                plus a 5 GB safety margin (
                <strong>{fmtMb(requiredMb)}</strong> total) won&rsquo;t
                fit. Free space detected:{" "}
                <strong>{fmtBytes(diskFreeBytes)}</strong>
                {diskFreePath ? (
                    <>
                        {" "}on{" "}
                        <code style={{ color: "inherit" }}>
                            {diskFreePath}
                        </code>
                    </>
                ) : null}
                . Free up{" "}
                <strong>{fmtBytes(shortfallBytes)}</strong> or pick a
                smaller model.
            </div>
        );
    }

    if (isFullyCached) {
        // All weights already on disk + headroom is sufficient.
        // Surface a small confirmation so the operator knows why no
        // warning is rendered for what looks like an "almost-full
        // disk" scenario (the cached bytes are doing the work).
        return (
            <div
                className="execlaw-muted small mt-2"
                data-testid={`${testIdPrefix}-disk-warning-cached`}
            >
                Model already cached ({fmtBytes(cachedBytes)}). No
                additional download required.
            </div>
        );
    }
    return null;
}
