// Phase 13.B.1 — managed-backend setup wizard.
//
// The Backends page exposes managed mode (Phase 12) but used to demand
// the operator hand-craft a `model_spec_json` blob. The wizard turns
// that into a card-pick UI: pull `/api/admin/backends/presets`,
// highlight the recommended preset based on detected hardware, render
// any per-preset configurable fields (today: Whisper model size,
// vLLM model id), and emit a fully-formed
// `(inference_backend, model_spec, gpu_id?)` triple the parent
// BackendsPage can save through the existing PUT route.
//
// Falls back to "show advanced JSON" when the operator wants to
// override the default args, image tag, or env. Locked-default
// presets ship in `crates/server/src/backend_presets.rs`.

import { useCallback, useEffect, useMemo, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    listBackendPresets,
    type BackendPurpose,
    type PresetField,
    type PresetsResponse,
    type PresetWithFlag,
} from "../api/endpoints";

export interface MaterialisedSpec {
    /// PluginId — matches one of the inference plugins. The wizard
    /// fixes this per preset family (vLLM presets → "service-vllm",
    /// Whisper presets → "service-whisper-stt", etc.). The parent
    /// passes it on to PUT /api/admin/backends/{purpose} verbatim.
    inference_backend: string;
    model_spec: {
        image: string;
        args: string[];
        container_port: number;
    };
    /// Operator-supplied GPU pin (sysfs id). Empty string → null.
    gpu_id: string;
}

export interface BackendWizardPanelProps {
    purpose: BackendPurpose;
    /// Auth callback shared with the rest of Settings.
    getToken: () => string | null;
    /// Called when the operator clicks "Use this preset". Parent
    /// then PUTs the row in managed mode.
    onApply: (spec: MaterialisedSpec) => void;
    /// Called when the operator wants to skip the wizard — e.g. an
    /// expert who'd rather type the JSON. Parent flips to the raw
    /// form.
    onSkip: () => void;
}

const VENDOR_LABEL: Record<string, string> = {
    nvidia: "NVIDIA",
    intel: "Intel Arc",
    amd: "AMD",
    cpu: "CPU",
};

/// Maps a preset id prefix to its inference-backend (PluginId). The
/// server doesn't need this — it's purely a SPA convenience to keep
/// the user from having to type `service-vllm` separately. Exported
/// for direct testing — the locked preset ids in
/// `crates/server/src/backend_presets.rs` must each match one of
/// these prefixes; an unmapped id silently falls through to
/// `service-managed`, which is invalid.
export function inferenceBackendFor(presetId: string): string {
    if (presetId.startsWith("vllm-")) return "service-vllm";
    if (presetId.startsWith("whisper-")) return "service-whisper-stt";
    if (presetId.startsWith("kokoro-")) return "service-kokoro-tts";
    if (presetId.startsWith("piper-")) return "service-piper-tts";
    return "service-managed";
}

/// Locally apply the same arg-template substitution the server does
/// in `materialise_spec`. The wizard sends a fully-formed
/// `model_spec` JSON straight to PUT, so this function — not the
/// server — is what actually shapes what the operator saves. The
/// server's `materialise_spec` exists for parity (and for any
/// future plugin-driven preset that bypasses the wizard); keeping
/// the two implementations in sync is on the developer.
function materialiseArgs(
    preset: PresetWithFlag,
    fieldValues: Record<string, string>,
): string[] {
    const out = [...preset.default_args];
    for (const f of preset.fields) {
        const value = fieldValues[f.kind] ?? f.default;
        if (f.arg_template.length > 0) {
            out.push(f.arg_template.replace("{value}", value));
        }
    }
    return out;
}

export function BackendWizardPanel({
    purpose,
    getToken,
    onApply,
    onSkip,
}: BackendWizardPanelProps) {
    const [data, setData] = useState<PresetsResponse | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [selectedId, setSelectedId] = useState<string | null>(null);
    const [fieldValues, setFieldValues] = useState<Record<string, string>>({});
    const [gpuId, setGpuId] = useState<string>("");
    const [showAdvanced, setShowAdvanced] = useState(false);
    const [overrideImage, setOverrideImage] = useState<string>("");

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await listBackendPresets(purpose, getToken);
                if (cancelled) return;
                setData(r);
                // Pre-select the recommended preset; if nothing is
                // recommended (e.g. AMD-only host or pre-server-bug-fix
                // unknown vendor), prefer the CPU preset over the
                // first card in the list — picking the CPU image on
                // an unknown-vendor host is always safer than picking
                // an NVIDIA image that won't run.
                const rec =
                    r.presets.find((p) => p.recommended) ??
                    r.presets.find((p) => p.vendor === "cpu") ??
                    r.presets[0];
                if (rec) {
                    setSelectedId(rec.id);
                    const initial: Record<string, string> = {};
                    for (const f of rec.fields) initial[f.kind] = f.default;
                    setFieldValues(initial);
                }
            } catch (e) {
                if (!cancelled) {
                    setError(e instanceof Error ? e.message : String(e));
                }
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [purpose, getToken]);

    const selected = useMemo(
        () => data?.presets.find((p) => p.id === selectedId) ?? null,
        [data, selectedId],
    );

    const onSelect = useCallback(
        (preset: PresetWithFlag) => {
            setSelectedId(preset.id);
            const next: Record<string, string> = {};
            for (const f of preset.fields) next[f.kind] = f.default;
            setFieldValues(next);
            setOverrideImage("");
        },
        [],
    );

    const onChangeField = useCallback(
        (field: PresetField, value: string) => {
            setFieldValues((prev) => ({ ...prev, [field.kind]: value }));
        },
        [],
    );

    const onUse = useCallback(() => {
        if (!selected) return;
        const args = materialiseArgs(selected, fieldValues);
        const image =
            overrideImage.trim().length > 0 ? overrideImage.trim() : selected.image;
        onApply({
            inference_backend: inferenceBackendFor(selected.id),
            model_spec: {
                image,
                args,
                container_port: selected.container_port,
            },
            gpu_id: gpuId.trim(),
        });
    }, [selected, fieldValues, gpuId, overrideImage, onApply]);

    if (error) {
        return (
            <div className="execlaw-error-banner mb-2" role="alert">
                Failed to load presets: {error}
            </div>
        );
    }

    if (data === null) {
        return (
            <div className="execlaw-muted small" data-testid="backend-wizard-loading">
                Loading presets…
            </div>
        );
    }

    return (
        <div data-testid="backend-wizard">
            <div className="execlaw-muted small mb-2">
                Detected hardware:{" "}
                {data.detected_vendors.length > 0 ? (
                    data.detected_vendors.map((v) => (
                        <span
                            key={v}
                            className="execlaw-trust-badge ms-1 is-known"
                            data-testid="backend-wizard-detected-vendor"
                        >
                            {VENDOR_LABEL[v] ?? v}
                        </span>
                    ))
                ) : (
                    <span
                        className="execlaw-trust-badge ms-1 is-limited"
                        data-testid="backend-wizard-detected-vendor"
                    >
                        No GPU
                    </span>
                )}
            </div>

            <div
                className="d-flex flex-column gap-2 mb-2"
                data-testid="backend-wizard-preset-list"
            >
                {data.presets.map((p) => (
                    <label
                        key={p.id}
                        className={
                            "execlaw-card mb-0 p-2 d-flex align-items-start gap-2 " +
                            (selectedId === p.id ? "border-primary" : "")
                        }
                        data-testid="backend-wizard-preset-card"
                        data-preset-id={p.id}
                        data-recommended={p.recommended ? "true" : "false"}
                    >
                        <Form.Check
                            type="radio"
                            name="backend-wizard-preset"
                            checked={selectedId === p.id}
                            onChange={() => onSelect(p)}
                            data-testid="backend-wizard-preset-radio"
                        />
                        <div className="flex-grow-1">
                            <div className="d-flex align-items-center gap-2 flex-wrap">
                                <strong>{p.name}</strong>
                                <span
                                    className={
                                        "execlaw-trust-badge " +
                                        (p.vendor === "cpu"
                                            ? "is-limited"
                                            : "is-known")
                                    }
                                >
                                    {VENDOR_LABEL[p.vendor] ?? p.vendor}
                                </span>
                                {p.recommended && (
                                    <span
                                        className="execlaw-trust-badge is-controller"
                                        data-testid="backend-wizard-recommended-badge"
                                    >
                                        Recommended
                                    </span>
                                )}
                            </div>
                            <div className="execlaw-muted small">{p.description}</div>
                            <div className="execlaw-muted small">
                                <code>{p.image}</code>
                            </div>
                        </div>
                    </label>
                ))}
            </div>

            {selected && selected.fields.length > 0 && (
                <div className="mb-2" data-testid="backend-wizard-fields">
                    {selected.fields.map((f) => (
                        <Form.Group key={f.kind} className="mb-2">
                            <Form.Label className="execlaw-muted small mb-1">
                                {f.label}
                            </Form.Label>
                            <Form.Select
                                value={fieldValues[f.kind] ?? f.default}
                                onChange={(e) => onChangeField(f, e.target.value)}
                                data-testid="backend-wizard-field"
                                data-field-kind={f.kind}
                            >
                                {f.choices.map((c) => (
                                    <option key={c} value={c}>
                                        {c}
                                    </option>
                                ))}
                            </Form.Select>
                        </Form.Group>
                    ))}
                </div>
            )}

            <Form.Group className="mb-2">
                <Form.Label className="execlaw-muted small mb-1">
                    GPU id (optional)
                </Form.Label>
                <Form.Control
                    value={gpuId}
                    onChange={(e) => setGpuId(e.target.value)}
                    placeholder="0"
                    data-testid="backend-wizard-gpu"
                />
                <Form.Text className="execlaw-muted">
                    Leave blank to let the supervisor pick the first GPU
                    matching this preset's vendor.
                </Form.Text>
            </Form.Group>

            <details
                className="mb-3"
                open={showAdvanced}
                onToggle={(e) =>
                    setShowAdvanced((e.target as HTMLDetailsElement).open)
                }
                data-testid="backend-wizard-advanced"
            >
                <summary className="execlaw-muted small">Show advanced</summary>
                <div className="mt-2">
                    <Form.Group className="mb-2">
                        <Form.Label className="execlaw-muted small mb-1">
                            Image override
                        </Form.Label>
                        <Form.Control
                            value={overrideImage}
                            onChange={(e) => setOverrideImage(e.target.value)}
                            placeholder={selected?.image ?? ""}
                            data-testid="backend-wizard-image-override"
                        />
                        <Form.Text className="execlaw-muted">
                            Pin a digest or use a private registry. Leave
                            blank to use the preset default.
                        </Form.Text>
                    </Form.Group>
                    {selected && (
                        <Form.Group>
                            <Form.Label className="execlaw-muted small mb-1">
                                Materialised spec (preview)
                            </Form.Label>
                            <pre
                                className="small mb-0"
                                data-testid="backend-wizard-args-preview"
                            >
                                {[
                                    `image: ${overrideImage.trim().length > 0 ? overrideImage.trim() : selected.image}`,
                                    `args:  ${materialiseArgs(selected, fieldValues).join(" ") || "(none)"}`,
                                    `port:  ${selected.container_port}`,
                                ].join("\n")}
                            </pre>
                        </Form.Group>
                    )}
                </div>
            </details>

            <div className="d-flex gap-2">
                <Button
                    variant="primary"
                    disabled={selected === null}
                    onClick={onUse}
                    data-testid="backend-wizard-apply"
                >
                    Use this preset
                </Button>
                <Button
                    variant="outline-secondary"
                    onClick={onSkip}
                    data-testid="backend-wizard-skip"
                >
                    I'll type the JSON
                </Button>
            </div>
        </div>
    );
}
