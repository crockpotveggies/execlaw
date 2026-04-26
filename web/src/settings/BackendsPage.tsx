// Settings → Backends (Phase 8.5; replaces the legacy
// "Deployments" CRUD UI).
//
// Renders the five fixed `BackendPurpose` values as a stable,
// edit-only list. Each row is either:
//   * **configured** — show backend + endpoint + GPU; "Edit" opens
//     the form pre-filled, "Clear" wipes the slot.
//   * **not configured** — show a placeholder; "Add backend" opens
//     the form blank.
//
// There is NO "+ New" affordance: the set of purposes is fixed by
// the runner architecture (Standard / Small /
// VoiceSTT / VoiceTTS). See docs/runner-design.md for why.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    BACKEND_PURPOSES,
    clearBackend,
    getBackendStatus,
    getHardware,
    listBackends,
    restartBackend,
    upsertBackend,
    type BackendListEntry,
    type BackendMode,
    type BackendPurpose,
    type BackendStatusResponse,
    type BackendView,
    type HardwareProfile,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import {
    BackendWizardPanel,
    type MaterialisedSpec,
} from "./BackendWizardPanel";

const PURPOSE_HINT: Record<BackendPurpose, string> = {
    Standard:
        "Default inference for chat turns. Engages reasoning mode (e.g. Qwen3.5 <think> blocks) when the toggle below is on.",
    Small:
        "Fast-path model for voice mode and any latency-sensitive route. The runner falls back to Standard when Small isn't configured.",
    VoiceSTT:
        "Speech-to-text — transcribes inbound audio for the voice pipeline.",
    VoiceTTS:
        "Text-to-speech — synthesises the runner's reply for voice calls.",
};

interface FormState {
    inference_backend: string;
    model_spec: string; // raw JSON the operator types
    gpu_id: string;
    endpoint: string;
    notes: string;
    reasoning_enabled: boolean;
    /// Phase 12 — lifecycle ownership. Default external preserves
    /// pre-Phase-12 behaviour; switching to managed makes the
    /// endpoint field a server-set output instead of an operator
    /// input.
    mode: BackendMode;
}

const EMPTY_FORM: FormState = {
    inference_backend: "",
    model_spec: '{"model": ""}',
    gpu_id: "",
    endpoint: "",
    notes: "",
    reasoning_enabled: false,
    mode: "external",
};

function fromBackend(b: BackendView): FormState {
    return {
        inference_backend: b.inference_backend,
        model_spec: JSON.stringify(b.model_spec, null, 2),
        gpu_id: b.gpu_id ?? "",
        endpoint: b.endpoint ?? "",
        notes: b.notes ?? "",
        reasoning_enabled: b.reasoning_enabled,
        mode: b.mode,
    };
}

const STATUS_BADGE: Record<string, string> = {
    Pulling: "is-pending",
    Starting: "is-pending",
    Healthy: "is-known",
    CrashLooping: "is-blocked",
    Stopped: "is-limited",
    NotFound: "is-limited",
};

export function BackendsPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [entries, setEntries] = useState<BackendListEntry[] | null>(null);
    const [statuses, setStatuses] = useState<
        Record<string, BackendStatusResponse>
    >({});
    const [error, setError] = useState<string | null>(null);
    const [editing, setEditing] = useState<BackendPurpose | null>(null);
    const [form, setForm] = useState<FormState>(EMPTY_FORM);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await listBackends(getToken);
            setEntries(r.backends);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    /// Pull live runtime status for every managed row. Cheap — one
    /// HTTP per managed purpose, four max. Polled every 5s while
    /// any managed row is configured.
    const refreshStatuses = useCallback(
        async (rows: BackendListEntry[]) => {
            const managed = rows.filter(
                (e) => e.backend && e.backend.mode === "managed",
            );
            if (managed.length === 0) {
                setStatuses({});
                return;
            }
            const next: Record<string, BackendStatusResponse> = {};
            for (const entry of managed) {
                try {
                    const s = await getBackendStatus(entry.purpose, getToken);
                    next[entry.purpose] = s;
                } catch {
                    // Silent — a transient probe failure shouldn't
                    // pollute the page. The pill will stay stale
                    // until the next tick.
                }
            }
            setStatuses(next);
        },
        [getToken],
    );

    useEffect(() => {
        void refresh();
    }, [refresh]);

    // Poll statuses whenever the entry list changes, then every 5s
    // for as long as at least one row is managed.
    useEffect(() => {
        if (!entries) return;
        void refreshStatuses(entries);
        const anyManaged = entries.some(
            (e) => e.backend && e.backend.mode === "managed",
        );
        if (!anyManaged) return;
        const id = window.setInterval(() => {
            void refreshStatuses(entries);
        }, 5_000);
        return () => window.clearInterval(id);
    }, [entries, refreshStatuses]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    /// True when the wizard should drive the form. We show it for
    /// fresh "Add backend" clicks (no prior config) and let the
    /// operator skip into raw JSON via the "I'll type the JSON" link.
    /// Editing an already-configured row goes straight to raw JSON
    /// since the operator's already past the picking phase.
    const [wizardActive, setWizardActive] = useState(false);

    const onEdit = (entry: BackendListEntry) => {
        setEditing(entry.purpose);
        setForm(entry.backend ? fromBackend(entry.backend) : EMPTY_FORM);
        setError(null);
        // Wizard surfaces only for fresh adds. Existing rows go to
        // raw JSON since the operator already picked.
        setWizardActive(!entry.configured);
    };

    const onCancel = () => {
        setEditing(null);
        setForm(EMPTY_FORM);
        setWizardActive(false);
    };

    /// Wizard → form bridge. The wizard hands us a fully-formed
    /// MaterialisedSpec; we drop it into the form's managed-mode
    /// fields, then flip into the raw-JSON view so the operator
    /// can review + click Save.
    const onWizardApply = useCallback((spec: MaterialisedSpec) => {
        setForm((prev) => ({
            ...prev,
            mode: "managed",
            inference_backend: spec.inference_backend,
            model_spec: JSON.stringify(spec.model_spec, null, 2),
            gpu_id: spec.gpu_id,
            // Managed mode — endpoint is server-managed.
            endpoint: "",
        }));
        setWizardActive(false);
    }, []);

    const onSave = useCallback(async () => {
        if (!editing) return;
        setBusy(true);
        setError(null);
        try {
            let parsedSpec: unknown;
            try {
                parsedSpec = JSON.parse(form.model_spec);
            } catch (e) {
                setError(
                    `model_spec must be valid JSON: ${e instanceof Error ? e.message : String(e)}`,
                );
                setBusy(false);
                return;
            }
            await upsertBackend(
                editing,
                {
                    inference_backend: form.inference_backend.trim(),
                    model_spec: parsedSpec,
                    gpu_id: form.gpu_id.trim().length > 0 ? form.gpu_id.trim() : null,
                    // For managed mode the operator doesn't supply
                    // an endpoint — the supervisor writes it back
                    // after spawn. We send null so a stale URL
                    // from a previous external mode isn't carried
                    // over.
                    endpoint:
                        form.mode === "managed"
                            ? null
                            : form.endpoint.trim().length > 0
                              ? form.endpoint.trim()
                              : null,
                    notes: form.notes.trim().length > 0 ? form.notes.trim() : null,
                    // Server zeroes this for non-Standard purposes;
                    // we still pass the form value through.
                    reasoning_enabled: form.reasoning_enabled,
                    mode: form.mode,
                },
                getToken,
            );
            setEditing(null);
            setForm(EMPTY_FORM);
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [editing, form, getToken, refresh]);

    const onClear = useCallback(
        async (purpose: BackendPurpose) => {
            if (
                !confirm(
                    `Clear the ${purpose} backend? Runners will fall back to the inference URL configured at process start.`,
                )
            )
                return;
            try {
                await clearBackend(purpose, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken, refresh],
    );

    const onRestart = useCallback(
        async (purpose: BackendPurpose) => {
            try {
                await restartBackend(purpose, getToken);
                // Surfaces in the next status poll; refresh now
                // for snappier UX.
                if (entries) await refreshStatuses(entries);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [entries, getToken, refreshStatuses],
    );

    return (
        <div data-testid="settings-backends">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Backends</h3>
                <Button
                    size="sm"
                    variant="outline-secondary"
                    onClick={() => void refresh()}
                    data-testid="backends-refresh"
                >
                    <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                    Refresh
                </Button>
            </div>

            <p className="execlaw-muted small mb-3">
                One inference backend per runner-purpose. Runners are
                spawned automatically per conversation and pick the
                backend matching their current modality / capability
                tier — see <code>Settings → Runners</code> for live
                runner state.
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can change backend
                    configuration.
                </div>
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {entries === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : (
                BACKEND_PURPOSES.map((purpose) => {
                    const entry = entries.find((e) => e.purpose === purpose) ?? {
                        purpose,
                        configured: false,
                        backend: null,
                    };
                    const isEditing = editing === purpose;
                    return (
                        <div
                            key={purpose}
                            className="execlaw-card"
                            data-testid="backend-row"
                            data-purpose={purpose}
                        >
                            <div className="d-flex align-items-center gap-2 mb-1 flex-wrap">
                                <span className="execlaw-card__title flex-grow-1">
                                    {purpose}
                                    {!entry.configured && (
                                        <span className="execlaw-trust-badge ms-2 is-limited">
                                            not configured
                                        </span>
                                    )}
                                    {entry.configured && (
                                        <span className="execlaw-trust-badge ms-2 is-controller">
                                            configured
                                        </span>
                                    )}
                                    {entry.backend?.mode === "managed" && (
                                        <span
                                            className="execlaw-trust-badge ms-2 is-known"
                                            data-testid="backend-mode-badge"
                                        >
                                            managed
                                        </span>
                                    )}
                                    {entry.backend?.reasoning_enabled && (
                                        <span
                                            className="execlaw-trust-badge ms-2 is-known"
                                            data-testid="backend-reasoning-badge"
                                        >
                                            reasoning on
                                        </span>
                                    )}
                                    {entry.backend?.mode === "managed" &&
                                        statuses[purpose] && (
                                            <span
                                                className={
                                                    "execlaw-trust-badge ms-2 " +
                                                    (STATUS_BADGE[
                                                        statuses[purpose].status
                                                    ] ?? "is-limited")
                                                }
                                                data-testid="backend-status-pill"
                                                title={
                                                    statuses[purpose]
                                                        .supervisor_available
                                                        ? "Reported by the BackendSupervisor"
                                                        : "Docker daemon unreachable — supervisor offline"
                                                }
                                            >
                                                {statuses[purpose].supervisor_available
                                                    ? statuses[purpose].status
                                                    : "Docker offline"}
                                            </span>
                                        )}
                                </span>
                                {canMutate && !isEditing && (
                                    <>
                                        <Button
                                            size="sm"
                                            variant="outline-primary"
                                            onClick={() => onEdit(entry)}
                                            data-testid="backend-edit"
                                        >
                                            {entry.configured ? "Edit" : "Add backend"}
                                        </Button>
                                        {entry.backend?.mode === "managed" &&
                                            statuses[purpose]?.supervisor_available && (
                                                <Button
                                                    size="sm"
                                                    variant="outline-warning"
                                                    onClick={() => void onRestart(purpose)}
                                                    data-testid="backend-restart"
                                                >
                                                    Restart
                                                </Button>
                                            )}
                                        {entry.configured && (
                                            <Button
                                                size="sm"
                                                variant="outline-danger"
                                                onClick={() => void onClear(purpose)}
                                                data-testid="backend-clear"
                                            >
                                                Clear
                                            </Button>
                                        )}
                                    </>
                                )}
                            </div>
                            <div className="execlaw-muted small mb-2">
                                {PURPOSE_HINT[purpose]}
                            </div>
                            {entry.backend && !isEditing && (
                                <div className="execlaw-muted small">
                                    <code>{entry.backend.inference_backend}</code>
                                    {entry.backend.endpoint && (
                                        <>
                                            {" → "}
                                            <code>{entry.backend.endpoint}</code>
                                        </>
                                    )}
                                    {entry.backend.gpu_id && (
                                        <>
                                            {" · GPU "}
                                            <code>{entry.backend.gpu_id}</code>
                                        </>
                                    )}
                                </div>
                            )}
                            {isEditing && wizardActive && (
                                <div
                                    className="mt-2"
                                    data-testid="backend-wizard-host"
                                >
                                    <BackendWizardPanel
                                        purpose={purpose}
                                        getToken={getToken}
                                        onApply={onWizardApply}
                                        onSkip={() => setWizardActive(false)}
                                    />
                                    <div className="mt-2">
                                        <Button
                                            variant="outline-secondary"
                                            size="sm"
                                            onClick={onCancel}
                                            data-testid="backend-wizard-cancel"
                                        >
                                            Cancel
                                        </Button>
                                    </div>
                                </div>
                            )}
                            {isEditing && !wizardActive && (
                                <div className="mt-2" data-testid="backend-form">
                                    <Form.Group className="mb-2">
                                        <Form.Label className="execlaw-muted small mb-1">
                                            Mode
                                        </Form.Label>
                                        <div
                                            className="d-flex gap-3"
                                            data-testid="backend-form-mode"
                                        >
                                            <Form.Check
                                                type="radio"
                                                id={`backend-form-mode-external-${purpose}`}
                                                name={`backend-form-mode-${purpose}`}
                                                label="External (operator-supplied URL)"
                                                checked={form.mode === "external"}
                                                onChange={() =>
                                                    setForm({ ...form, mode: "external" })
                                                }
                                                data-testid="backend-form-mode-external"
                                            />
                                            <Form.Check
                                                type="radio"
                                                id={`backend-form-mode-managed-${purpose}`}
                                                name={`backend-form-mode-${purpose}`}
                                                label="Managed (control plane runs the container)"
                                                checked={form.mode === "managed"}
                                                onChange={() =>
                                                    setForm({ ...form, mode: "managed" })
                                                }
                                                data-testid="backend-form-mode-managed"
                                            />
                                        </div>
                                        <Form.Text className="execlaw-muted">
                                            {form.mode === "managed"
                                                ? "Endpoint is set by the supervisor after spawn. Provide the image tag + args in the model_spec JSON below."
                                                : "Operator-managed endpoint. Configure the URL below."}
                                        </Form.Text>
                                    </Form.Group>
                                    <Form.Group className="mb-2">
                                        <Form.Label className="execlaw-muted small mb-1">
                                            Inference backend (PluginId)
                                        </Form.Label>
                                        <Form.Control
                                            value={form.inference_backend}
                                            onChange={(e) =>
                                                setForm({
                                                    ...form,
                                                    inference_backend: e.target.value,
                                                })
                                            }
                                            placeholder="service-vllm"
                                            data-testid="backend-form-backend"
                                        />
                                    </Form.Group>
                                    <Form.Group className="mb-2">
                                        <Form.Label className="execlaw-muted small mb-1">
                                            Model spec (JSON)
                                        </Form.Label>
                                        <Form.Control
                                            as="textarea"
                                            rows={4}
                                            value={form.model_spec}
                                            onChange={(e) =>
                                                setForm({
                                                    ...form,
                                                    model_spec: e.target.value,
                                                })
                                            }
                                            spellCheck={false}
                                            data-testid="backend-form-model-spec"
                                        />
                                    </Form.Group>
                                    <div className="row g-2 mb-2">
                                        <Form.Group className="col-sm-6">
                                            <Form.Label className="execlaw-muted small mb-1">
                                                GPU id (optional)
                                            </Form.Label>
                                            <Form.Control
                                                value={form.gpu_id}
                                                onChange={(e) =>
                                                    setForm({
                                                        ...form,
                                                        gpu_id: e.target.value,
                                                    })
                                                }
                                                placeholder="0"
                                                data-testid="backend-form-gpu"
                                            />
                                        </Form.Group>
                                        <Form.Group className="col-sm-6">
                                            <Form.Label className="execlaw-muted small mb-1">
                                                Endpoint{" "}
                                                {form.mode === "managed"
                                                    ? "(server-managed)"
                                                    : "(optional)"}
                                            </Form.Label>
                                            <Form.Control
                                                value={
                                                    form.mode === "managed"
                                                        ? "(set after container spawn)"
                                                        : form.endpoint
                                                }
                                                onChange={(e) =>
                                                    setForm({
                                                        ...form,
                                                        endpoint: e.target.value,
                                                    })
                                                }
                                                placeholder="http://127.0.0.1:8000/v1"
                                                disabled={form.mode === "managed"}
                                                data-testid="backend-form-endpoint"
                                            />
                                        </Form.Group>
                                    </div>
                                    <Form.Group className="mb-3">
                                        <Form.Label className="execlaw-muted small mb-1">
                                            Notes
                                        </Form.Label>
                                        <Form.Control
                                            as="textarea"
                                            rows={2}
                                            value={form.notes}
                                            onChange={(e) =>
                                                setForm({
                                                    ...form,
                                                    notes: e.target.value,
                                                })
                                            }
                                            data-testid="backend-form-notes"
                                        />
                                    </Form.Group>
                                    {/* Reasoning toggle is Standard-only.
                                        Other purposes don't expose a
                                        reasoning concept; the server
                                        silently zeroes the value if the
                                        SPA somehow sends it. */}
                                    {purpose === "Standard" && (
                                        <Form.Check
                                            type="switch"
                                            id="backend-form-reasoning"
                                            label="Engage reasoning mode (e.g. Qwen3.5 <think> blocks)"
                                            checked={form.reasoning_enabled}
                                            onChange={(e) =>
                                                setForm({
                                                    ...form,
                                                    reasoning_enabled:
                                                        e.target.checked,
                                                })
                                            }
                                            className="mb-3"
                                            data-testid="backend-form-reasoning"
                                        />
                                    )}
                                    <div className="d-flex gap-2">
                                        <Button
                                            variant="primary"
                                            disabled={
                                                busy ||
                                                form.inference_backend.trim().length === 0
                                            }
                                            onClick={() => void onSave()}
                                            data-testid="backend-form-save"
                                        >
                                            Save
                                        </Button>
                                        <Button
                                            variant="outline-secondary"
                                            onClick={onCancel}
                                            data-testid="backend-form-cancel"
                                        >
                                            Cancel
                                        </Button>
                                        {form.mode === "managed" && (
                                            <Button
                                                variant="outline-secondary"
                                                onClick={() => setWizardActive(true)}
                                                data-testid="backend-form-rerun-wizard"
                                            >
                                                Re-run wizard
                                            </Button>
                                        )}
                                    </div>
                                </div>
                            )}
                        </div>
                    );
                })
            )}

            <HardwareSection />
        </div>
    );
}

// ---------------------------------------------------------------------------
// Hardware section — fetches /api/admin/hardware and renders the
// detected GPU profile inline at the bottom of the Backends page.
// Read-only; the operator wires GPU ids into individual backend
// rows above. Replaces the standalone Hardware tab.
// ---------------------------------------------------------------------------

function HardwareSection() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [profile, setProfile] = useState<HardwareProfile | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await getHardware(getToken);
                if (!cancelled) setProfile(r);
            } catch (e) {
                if (!cancelled)
                    setError(e instanceof Error ? e.message : String(e));
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    return (
        <div className="mt-4" data-testid="settings-hardware">
            <div className="d-flex align-items-center mb-2">
                <h4 className="h6 mb-0 flex-grow-1">Hardware</h4>
            </div>
            <p className="execlaw-muted small mb-2">
                Detected GPUs and their PCI ids. Use the GPU id values
                above when configuring per-backend pinning.
            </p>

            {error && (
                <div className="execlaw-error-banner mb-2" role="alert">
                    {error}
                </div>
            )}

            {profile === null ? (
                <div className="execlaw-muted small">Probing hardware…</div>
            ) : (
                <>
                    <GpuList gpus={Array.isArray(profile.gpus) ? profile.gpus : []} />
                    <details className="execlaw-card">
                        <summary className="execlaw-muted small">
                            Raw profile JSON
                        </summary>
                        <pre className="mt-2 mb-0 small">
                            {JSON.stringify(profile, null, 2)}
                        </pre>
                    </details>
                </>
            )}
        </div>
    );
}

function GpuList({ gpus }: { gpus: HardwareProfile["gpus"] & object[] }) {
    if (!gpus || gpus.length === 0) {
        return (
            <div className="execlaw-card">
                <div className="execlaw-card__title">GPUs</div>
                <div className="execlaw-muted small">
                    No GPUs detected. The control plane is running CPU-only
                    — inference plugins that need a GPU will be unavailable.
                </div>
            </div>
        );
    }
    return (
        <div className="execlaw-card">
            <div className="execlaw-card__title">GPUs ({gpus.length})</div>
            {gpus.map((g, i) => (
                <div key={i} className="execlaw-card__row">
                    <div>
                        <div>
                            <strong>
                                {(g.vendor as string) ?? "GPU"}{" "}
                                {(g.model as string) ?? ""}
                            </strong>
                        </div>
                        <div className="execlaw-muted small">
                            {(g.pci_vendor_id as string | undefined) ?? "?"}:
                            {(g.pci_device_id as string | undefined) ?? "?"}
                        </div>
                    </div>
                </div>
            ))}
        </div>
    );
}
