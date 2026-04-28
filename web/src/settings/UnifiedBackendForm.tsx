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
import {
    type BackendPurpose,
    type DetectedGpu,
    type UpsertBackendRequest,
} from "../api/endpoints";

export type ServingMethod = "vllm" | "openvino" | "openarc";

interface ModelOption {
    /// Hugging Face repo id / OpenVINO model id — written into
    /// `--model={id}` for the chosen serving image.
    id: string;
    /// Display label in the dropdown.
    label: string;
    /// Approximate minimum VRAM required, in MiB. The form hides
    /// entries that exceed the picked GPU's `memory_mb`.
    min_mb: number;
}

const SERVING_LABEL: Record<ServingMethod, string> = {
    vllm: "vLLM",
    openvino: "OpenVINO model server",
    openarc: "OpenArc",
};

const SERVING_PLUGIN: Record<ServingMethod, string> = {
    vllm: "service-vllm",
    openvino: "service-vllm-openvino-arc",
    openarc: "service-openarc",
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
    /// Whether the host has a reachable Docker daemon. When false
    /// the GPU targets are hidden and only Remote is offered (since
    /// managed mode would have nowhere to spawn).
    dockerAvailable: boolean;
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

export function UnifiedBackendForm({
    purpose,
    gpus,
    dockerAvailable,
    onSubmit,
    onSkip,
    submitLabel = "Save backend",
    skipLabel = "Skip for now",
    testIdPrefix = "unified-backend",
}: UnifiedBackendFormProps) {
    const targets: Target[] = [];
    if (dockerAvailable) {
        gpus.forEach((g, idx) => {
            if (servingMethodsFor(g).length > 0) {
                targets.push({ kind: "gpu", gpu: g, gpuIdx: idx });
            }
        });
    }
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
            {submitError && (
                <div
                    className="execlaw-error-banner mb-3"
                    role="alert"
                    data-testid={`${testIdPrefix}-error`}
                >
                    {submitError}
                </div>
            )}
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
                                </>
                            )}
                        </Form.Group>
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
                        disabled={submitting}
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
        default:
            return undefined;
    }
}
