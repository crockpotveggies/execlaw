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
// the runner architecture (Standard / Reasoning / Guardrail /
// VoiceSTT / VoiceTTS). See docs/runner-design.md for why.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    BACKEND_PURPOSES,
    clearBackend,
    listBackends,
    upsertBackend,
    type BackendListEntry,
    type BackendPurpose,
    type BackendView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

const PURPOSE_HINT: Record<BackendPurpose, string> = {
    Standard:
        "Default inference for chat turns. The runner picks this when no other purpose applies.",
    Reasoning:
        "Heavier model used when the runner needs deeper reasoning (research orchestrator, hard tool-calls).",
    Guardrail:
        "Lightweight model that screens runner output before it leaves the conversation.",
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
}

const EMPTY_FORM: FormState = {
    inference_backend: "",
    model_spec: '{"model": ""}',
    gpu_id: "",
    endpoint: "",
    notes: "",
};

function fromBackend(b: BackendView): FormState {
    return {
        inference_backend: b.inference_backend,
        model_spec: JSON.stringify(b.model_spec, null, 2),
        gpu_id: b.gpu_id ?? "",
        endpoint: b.endpoint ?? "",
        notes: b.notes ?? "",
    };
}

export function BackendsPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [entries, setEntries] = useState<BackendListEntry[] | null>(null);
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

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    const onEdit = (entry: BackendListEntry) => {
        setEditing(entry.purpose);
        setForm(entry.backend ? fromBackend(entry.backend) : EMPTY_FORM);
        setError(null);
    };

    const onCancel = () => {
        setEditing(null);
        setForm(EMPTY_FORM);
    };

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
                    endpoint:
                        form.endpoint.trim().length > 0 ? form.endpoint.trim() : null,
                    notes: form.notes.trim().length > 0 ? form.notes.trim() : null,
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
                            <div className="d-flex align-items-center gap-2 mb-1">
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
                            {isEditing && (
                                <div className="mt-2" data-testid="backend-form">
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
                                                Endpoint (optional)
                                            </Form.Label>
                                            <Form.Control
                                                value={form.endpoint}
                                                onChange={(e) =>
                                                    setForm({
                                                        ...form,
                                                        endpoint: e.target.value,
                                                    })
                                                }
                                                placeholder="http://127.0.0.1:8000/v1"
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
                                    </div>
                                </div>
                            )}
                        </div>
                    );
                })
            )}
        </div>
    );
}
