// Deployments page — Phase 7 surface for `config_runner_deployments`
// CRUD. Lists every deployment grouped by purpose, lets the operator
// create / edit / delete rows, and surfaces is_default + active flags.
//
// Uncomplicated form: a single inline editor per row. The model_spec
// field is a JSON textarea — we parse it client-side before
// submitting so the operator gets immediate feedback rather than a
// 400 from the server.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    createDeployment,
    deleteDeployment,
    listDeployments,
    updateDeployment,
    type DeploymentPurpose,
    type DeploymentView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

const PURPOSES: ReadonlyArray<DeploymentPurpose> = [
    "Standard",
    "Reasoning",
    "Guardrail",
    "VoiceSTT",
    "VoiceTTS",
];

interface FormState {
    purpose: DeploymentPurpose;
    inference_backend: string;
    model_spec: string;
    gpu_id: string;
    endpoint: string;
    is_default: boolean;
    active: boolean;
    notes: string;
}

const EMPTY_FORM: FormState = {
    purpose: "Standard",
    inference_backend: "service-vllm",
    model_spec: '{\n  "model": "Qwen3.5-27B-AWQ"\n}',
    gpu_id: "",
    endpoint: "http://127.0.0.1:8000/v1",
    is_default: true,
    active: true,
    notes: "",
};

export function DeploymentsPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const [deployments, setDeployments] = useState<DeploymentView[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [creating, setCreating] = useState(false);
    const [createForm, setCreateForm] = useState<FormState>(EMPTY_FORM);
    const [createError, setCreateError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listDeployments(getToken);
            setDeployments(r.deployments);
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onCreate = useCallback(
        async (e: React.FormEvent) => {
            e.preventDefault();
            setCreateError(null);
            let modelSpec: unknown;
            try {
                modelSpec = JSON.parse(createForm.model_spec);
            } catch (err) {
                setCreateError(
                    `model_spec must be valid JSON: ${
                        err instanceof Error ? err.message : String(err)
                    }`,
                );
                return;
            }
            try {
                await createDeployment(
                    {
                        purpose: createForm.purpose,
                        inference_backend: createForm.inference_backend.trim(),
                        model_spec: modelSpec,
                        gpu_id: createForm.gpu_id.trim() || null,
                        endpoint: createForm.endpoint.trim() || null,
                        is_default: createForm.is_default,
                        active: createForm.active,
                        notes: createForm.notes.trim() || null,
                    },
                    getToken,
                );
                setCreateForm(EMPTY_FORM);
                setCreating(false);
                await refresh();
            } catch (err) {
                setCreateError(
                    err instanceof Error ? err.message : String(err),
                );
            }
        },
        [createForm, getToken, refresh],
    );

    const onPatch = useCallback(
        async (
            id: string,
            patch: Parameters<typeof updateDeployment>[1],
        ) => {
            setBusyId(id);
            try {
                await updateDeployment(id, patch, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    const onDelete = useCallback(
        async (d: DeploymentView) => {
            if (
                !confirm(
                    `Delete deployment ${d.id} (${d.purpose} · ${d.inference_backend})? This is permanent.`,
                )
            )
                return;
            setBusyId(d.id);
            try {
                await deleteDeployment(d.id, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh],
    );

    return (
        <div data-testid="settings-deployments">
            <div className="d-flex align-items-center mb-3">
                <h3 className="h6 mb-0 flex-grow-1">Runner deployments</h3>
                {!creating && (
                    <Button
                        size="sm"
                        variant="primary"
                        onClick={() => {
                            setCreating(true);
                            setCreateError(null);
                            setCreateForm(EMPTY_FORM);
                        }}
                        data-testid="deployments-new"
                    >
                        <i className="bi bi-plus-lg me-2" aria-hidden />
                        New deployment
                    </Button>
                )}
            </div>

            {creating && (
                <CreateForm
                    form={createForm}
                    setForm={setCreateForm}
                    error={createError}
                    onCancel={() => {
                        setCreating(false);
                        setCreateError(null);
                    }}
                    onSubmit={onCreate}
                />
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {deployments === null ? (
                <div className="execlaw-muted small">Loading deployments…</div>
            ) : deployments.length === 0 ? (
                <div className="execlaw-muted small">
                    No deployments configured. The runner falls back to the
                    default Standard deployment described in
                    MIGRATION_PLAN §5.4 until you add one.
                </div>
            ) : (
                deployments.map((d) => (
                    <DeploymentCard
                        key={d.id}
                        deployment={d}
                        busy={busyId === d.id}
                        onPatch={(patch) => void onPatch(d.id, patch)}
                        onDelete={() => void onDelete(d)}
                    />
                ))
            )}
        </div>
    );
}

interface CreateFormProps {
    form: FormState;
    setForm: (next: FormState) => void;
    error: string | null;
    onCancel: () => void;
    onSubmit: (e: React.FormEvent) => void;
}

function CreateForm({ form, setForm, error, onCancel, onSubmit }: CreateFormProps) {
    return (
        <form
            className="execlaw-card"
            onSubmit={onSubmit}
            data-testid="deployments-create-form"
        >
            <div className="execlaw-card__title mb-3">New deployment</div>
            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}
            <div className="row g-2">
                <Form.Group className="col-sm-4">
                    <Form.Label className="execlaw-muted small mb-1">
                        Purpose
                    </Form.Label>
                    <Form.Select
                        value={form.purpose}
                        onChange={(e) =>
                            setForm({
                                ...form,
                                purpose: e.target.value as DeploymentPurpose,
                            })
                        }
                        data-testid="deployments-purpose"
                    >
                        {PURPOSES.map((p) => (
                            <option key={p} value={p}>
                                {p}
                            </option>
                        ))}
                    </Form.Select>
                </Form.Group>
                <Form.Group className="col-sm-4">
                    <Form.Label className="execlaw-muted small mb-1">
                        Inference backend
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
                        data-testid="deployments-backend"
                    />
                </Form.Group>
                <Form.Group className="col-sm-4">
                    <Form.Label className="execlaw-muted small mb-1">
                        Endpoint
                    </Form.Label>
                    <Form.Control
                        value={form.endpoint}
                        onChange={(e) =>
                            setForm({ ...form, endpoint: e.target.value })
                        }
                        placeholder="http://127.0.0.1:8000/v1"
                    />
                </Form.Group>
                <Form.Group className="col-sm-4">
                    <Form.Label className="execlaw-muted small mb-1">
                        GPU id
                    </Form.Label>
                    <Form.Control
                        value={form.gpu_id}
                        onChange={(e) =>
                            setForm({ ...form, gpu_id: e.target.value })
                        }
                        placeholder="optional"
                    />
                </Form.Group>
                <Form.Group className="col-sm-4 d-flex align-items-end gap-3">
                    <Form.Check
                        type="switch"
                        id="dep-is-default"
                        label="Default for purpose"
                        checked={form.is_default}
                        onChange={(e) =>
                            setForm({ ...form, is_default: e.target.checked })
                        }
                    />
                    <Form.Check
                        type="switch"
                        id="dep-active"
                        label="Active"
                        checked={form.active}
                        onChange={(e) =>
                            setForm({ ...form, active: e.target.checked })
                        }
                    />
                </Form.Group>
                <Form.Group className="col-12">
                    <Form.Label className="execlaw-muted small mb-1">
                        Model spec (JSON)
                    </Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={5}
                        value={form.model_spec}
                        onChange={(e) =>
                            setForm({ ...form, model_spec: e.target.value })
                        }
                        spellCheck={false}
                        style={{
                            fontFamily:
                                "ui-monospace, 'SF Mono', Menlo, Consolas, monospace",
                            fontSize: "0.85rem",
                        }}
                        data-testid="deployments-model-spec"
                    />
                </Form.Group>
                <Form.Group className="col-12">
                    <Form.Label className="execlaw-muted small mb-1">
                        Notes
                    </Form.Label>
                    <Form.Control
                        value={form.notes}
                        onChange={(e) =>
                            setForm({ ...form, notes: e.target.value })
                        }
                        placeholder="optional"
                    />
                </Form.Group>
            </div>
            <div className="d-flex gap-2 mt-3">
                <Button
                    type="submit"
                    variant="primary"
                    data-testid="deployments-create-submit"
                >
                    Create
                </Button>
                <Button variant="outline-secondary" onClick={onCancel}>
                    Cancel
                </Button>
            </div>
        </form>
    );
}

interface DeploymentCardProps {
    deployment: DeploymentView;
    busy: boolean;
    onPatch: (patch: Parameters<typeof updateDeployment>[1]) => void;
    onDelete: () => void;
}

function DeploymentCard({
    deployment,
    busy,
    onPatch,
    onDelete,
}: DeploymentCardProps) {
    return (
        <div className="execlaw-card" data-testid="deployments-card">
            <div className="d-flex align-items-center gap-2 mb-2">
                <span className="execlaw-trust-badge">{deployment.purpose}</span>
                <span className="execlaw-card__title flex-grow-1">
                    {deployment.inference_backend}
                </span>
                {deployment.is_default && (
                    <span className="execlaw-trust-badge is-controller">
                        default
                    </span>
                )}
                {!deployment.active && (
                    <span className="execlaw-trust-badge is-blocked">
                        inactive
                    </span>
                )}
            </div>
            <div className="execlaw-muted small mb-1">
                <code>{deployment.id}</code>
                {deployment.gpu_id && (
                    <>
                        {" · GPU "}
                        <code>{deployment.gpu_id}</code>
                    </>
                )}
                {deployment.endpoint && (
                    <>
                        {" · "}
                        <code>{deployment.endpoint}</code>
                    </>
                )}
            </div>
            {deployment.notes && (
                <div className="small mb-2">{deployment.notes}</div>
            )}
            <details className="mb-2">
                <summary className="execlaw-muted small">Model spec</summary>
                <pre className="small mb-0 mt-1">
                    {JSON.stringify(deployment.model_spec, null, 2)}
                </pre>
            </details>
            <div className="d-flex gap-2 flex-wrap">
                <Button
                    size="sm"
                    variant={deployment.is_default ? "outline-secondary" : "outline-primary"}
                    disabled={busy || deployment.is_default}
                    onClick={() => onPatch({ is_default: true })}
                    data-testid="deployments-make-default"
                >
                    <i className="bi bi-star me-2" aria-hidden />
                    Make default
                </Button>
                <Button
                    size="sm"
                    variant={deployment.active ? "outline-warning" : "outline-success"}
                    disabled={busy}
                    onClick={() => onPatch({ active: !deployment.active })}
                    data-testid="deployments-toggle-active"
                >
                    {deployment.active ? "Deactivate" : "Activate"}
                </Button>
                <Button
                    size="sm"
                    variant="outline-danger"
                    disabled={busy}
                    onClick={onDelete}
                    data-testid="deployments-delete"
                >
                    Delete
                </Button>
            </div>
        </div>
    );
}
