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
import Modal from "react-bootstrap/Modal";
import {
    BACKEND_PURPOSES,
    clearBackend,
    getBackendLogs,
    getBackendStatus,
    getHardware,
    getHfCache,
    getSetupPreflight,
    listBackends,
    putHfCache,
    restartBackend,
    upsertBackend,
    type BackendListEntry,
    type BackendLogsResponse,
    type BackendMode,
    type BackendPurpose,
    type BackendStatusResponse,
    type BackendView,
    type DetectedGpu,
    type HardwareProfile,
    type UpsertBackendRequest,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import { UnifiedBackendForm } from "./UnifiedBackendForm";

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

/// User-facing label by lifecycle STAGE (preferred) — falls back
/// to the legacy STATUS lookup for older server builds that don't
/// populate `stage`. Each label is short enough to fit in a pill.
const STAGE_LABEL: Record<string, string> = {
    Idle: "Stopped",
    DownloadingModel: "Downloading model",
    PullingImage: "Pulling image",
    ContainerStarting: "Starting container",
    LoadingModel: "Loading model",
    Healthy: "Healthy",
    Failed: "Crash-looping",
};

const STAGE_TITLE: Record<string, string> = {
    Idle: "The supervisor isn't running this container right now.",
    DownloadingModel:
        "The host-side downloader is fetching model weights from huggingface.co into ~/.execlaw/hf-cache/. This happens once per model — subsequent spawns reuse the cache. The container is NOT yet running.",
    PullingImage:
        "Bollard is downloading the container image from the registry. First spawn can pull 30+ GB for the vLLM nightly tag — typically 5–15 minutes on a home connection.",
    ContainerStarting:
        "Container is up; waiting for the inference service to start its TCP listener.",
    LoadingModel:
        "The container is up and the inference service has started. It's now loading model weights into VRAM. For Qwen 3.5 27B AWQ this is typically 1–3 minutes after the container starts.",
    Healthy: "Reported by the BackendSupervisor",
    Failed:
        "The supervisor restarted this container repeatedly without success. Click Logs for the captured error, or check Settings → Alerts.",
};

/// Render bytes as a short "8.5 GB" / "342 MB" / "12.0 KB" string.
function fmtBytes(b: number): string {
    if (b < 1024) return `${b} B`;
    if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
    if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB`;
    return `${(b / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

/// Legacy STATUS-keyed labels for back-compat with builds that
/// pre-date the `stage` field. New code should prefer STAGE_LABEL
/// because "Provisioning" is uninformative during a 5-minute load.
const STATUS_LABEL: Record<string, string> = {
    Pulling: "Pulling image",
    Starting: "Starting",
    Healthy: "Healthy",
    CrashLooping: "Crash-looping",
    Stopped: "Stopped",
    NotFound: "Not found",
};

/// Legacy STATUS-keyed tooltips, used as a fallback when the
/// server hasn't populated `stage` yet (older binary still
/// running while the SPA is hot-reloaded ahead of it).
const STATUS_TITLE: Record<string, string> = {
    Pulling:
        "Pulling the container image. First spawn of a managed backend can take several minutes — the image weight is typically 5–15 GB.",
    Starting:
        "Container is up; waiting for the inference service to report Healthy. vLLM warm-up + model load can take 1–3 minutes after the pull finishes.",
    Healthy: "Reported by the BackendSupervisor",
    CrashLooping:
        "The supervisor restarted this container repeatedly without success. Click Logs for the captured error, or check Settings → Alerts.",
    Stopped: "The supervisor isn't running this container right now.",
    NotFound: "Container vanished — the supervisor will respawn on the next reconcile tick.",
};

/// Render an elapsed-seconds count as a short "4m20s" / "1h3m"
/// string. Used in the pill's hover title + the inline detail.
function fmtElapsed(secs: number | null): string {
    if (secs === null || secs <= 0) return "";
    if (secs < 60) return `${secs}s`;
    const m = Math.floor(secs / 60);
    if (m < 60) return `${m}m${secs % 60}s`;
    const h = Math.floor(m / 60);
    return `${h}h${m % 60}m`;
}

export function BackendsPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [entries, setEntries] = useState<BackendListEntry[] | null>(null);
    const [statuses, setStatuses] = useState<
        Record<string, BackendStatusResponse>
    >({});
    const [error, setError] = useState<string | null>(null);
    const [editing, setEditing] = useState<BackendPurpose | null>(null);
    const [form, setForm] = useState<FormState>(EMPTY_FORM);
    const [busy, setBusy] = useState(false);
    // Phase 14 follow-up — the inline wizard now uses the same
    // UnifiedBackendForm the first-run /setup wizard renders. That
    // form needs the live GPU + Docker-availability snapshot so it
    // can list Intel Arc + NVIDIA targets and filter the model
    // catalog by VRAM. We reuse the same `/api/admin/setup/preflight`
    // probe so there's no second source of truth.
    const [preflightGpus, setPreflightGpus] = useState<DetectedGpu[]>([]);
    const [preflightDockerOk, setPreflightDockerOk] = useState<boolean>(true);
    const [preflightDiskFree, setPreflightDiskFree] = useState<number | null>(null);
    const [preflightDiskPath, setPreflightDiskPath] = useState<string | null>(null);
    const [preflightLoaded, setPreflightLoaded] = useState<boolean>(false);
    /// Container-logs modal state. Surfaces the supervisor's
    /// captured tail (last 200 lines) so the operator can diagnose
    /// CrashLooping without dropping into a Docker shell.
    const [logsModal, setLogsModal] = useState<
        | {
              purpose: BackendPurpose;
              loading: boolean;
              data: BackendLogsResponse | null;
              error: string | null;
          }
        | null
    >(null);

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

    /// Pull the GPU + Docker preflight on mount so the inline wizard
    /// has hardware data to render. We don't block the page on this:
    /// if it fails (e.g. probe service hiccup), the wizard falls back
    /// to "no GPU detected" which still surfaces the Remote-endpoint
    /// option and lets the operator type a URL.
    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await getSetupPreflight(getToken);
                if (cancelled) return;
                setPreflightGpus(r.gpus);
                setPreflightDockerOk(r.docker.available);
                setPreflightDiskFree(r.disk_free_bytes ?? null);
                setPreflightDiskPath(r.disk_free_path ?? null);
            } catch {
                if (cancelled) return;
                setPreflightGpus([]);
                setPreflightDockerOk(false);
                setPreflightDiskFree(null);
                setPreflightDiskPath(null);
            } finally {
                if (!cancelled) setPreflightLoaded(true);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

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

    /// The UnifiedBackendForm submits an UpsertBackendRequest
    /// directly. We delegate to the existing upsert + refresh path
    /// so the page transitions out of edit mode + repolls statuses
    /// the same way a manual JSON save does.
    const onWizardSubmit = useCallback(
        async (purpose: BackendPurpose, body: UpsertBackendRequest) => {
            await upsertBackend(purpose, body, getToken);
            setEditing(null);
            setForm(EMPTY_FORM);
            setWizardActive(false);
            await refresh();
        },
        [getToken, refresh],
    );

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

    /// Open the logs modal for a given purpose, then fetch the tail.
    /// Async errors render inside the modal rather than the page so
    /// the operator's mental context (which row they clicked on)
    /// stays intact.
    const onViewLogs = useCallback(
        async (purpose: BackendPurpose) => {
            setLogsModal({ purpose, loading: true, data: null, error: null });
            try {
                const data = await getBackendLogs(purpose, 200, getToken);
                setLogsModal({
                    purpose,
                    loading: false,
                    data,
                    error: null,
                });
            } catch (e) {
                setLogsModal({
                    purpose,
                    loading: false,
                    data: null,
                    error: e instanceof Error ? e.message : String(e),
                });
            }
        },
        [getToken],
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

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

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
                                        statuses[purpose] &&
                                        (() => {
                                            const s = statuses[purpose];
                                            // Prefer the high-resolution stage
                                            // (server ≥ Phase 14.B). Fall back to
                                            // the legacy `status` enum so an old
                                            // server still renders something
                                            // meaningful.
                                            const stage = s.stage ?? null;
                                            const label =
                                                (stage && STAGE_LABEL[stage]) ??
                                                STATUS_LABEL[s.status] ??
                                                s.status;
                                            const tooltip =
                                                (stage && STAGE_TITLE[stage]) ??
                                                STATUS_TITLE[s.status] ??
                                                "Reported by the BackendSupervisor";
                                            const inProgress =
                                                stage === "DownloadingModel" ||
                                                stage === "PullingImage" ||
                                                stage === "ContainerStarting" ||
                                                stage === "LoadingModel" ||
                                                s.status === "Pulling" ||
                                                s.status === "Starting";
                                            const elapsed = fmtElapsed(s.elapsed_secs);
                                            const showElapsed =
                                                inProgress && elapsed.length > 0;
                                            const dl = s.download_progress;
                                            const dlPct =
                                                dl && dl.total_bytes > 0
                                                    ? Math.floor(
                                                          (dl.bytes_downloaded * 100) /
                                                              dl.total_bytes,
                                                      )
                                                    : null;
                                            return (
                                                <span
                                                    className={
                                                        "execlaw-trust-badge ms-2 " +
                                                        (STATUS_BADGE[s.status] ??
                                                            "is-limited")
                                                    }
                                                    data-testid="backend-status-pill"
                                                    data-status={s.status}
                                                    data-stage={stage ?? ""}
                                                    title={
                                                        s.supervisor_available
                                                            ? `${tooltip}${
                                                                  s.last_log_line
                                                                      ? `\n\nLast log line: ${s.last_log_line}`
                                                                      : ""
                                                              }${
                                                                  showElapsed
                                                                      ? `\nElapsed: ${elapsed}`
                                                                      : ""
                                                              }`
                                                            : "Docker daemon unreachable — supervisor offline"
                                                    }
                                                >
                                                    {s.supervisor_available ? (
                                                        <>
                                                            {inProgress && (
                                                                <span
                                                                    className="execlaw-spinner-dot me-1"
                                                                    aria-hidden
                                                                />
                                                            )}
                                                            {label}
                                                            {dl && dlPct !== null && (
                                                                <span
                                                                    className="ms-1 execlaw-muted"
                                                                    data-testid="backend-download-progress"
                                                                >
                                                                    · {fmtBytes(
                                                                        dl.bytes_downloaded,
                                                                    )}
                                                                    {" / "}
                                                                    {fmtBytes(
                                                                        dl.total_bytes,
                                                                    )}
                                                                    {" · "}
                                                                    {dlPct}%
                                                                </span>
                                                            )}
                                                            {showElapsed && (
                                                                <span
                                                                    className="ms-1 execlaw-muted"
                                                                    data-testid="backend-elapsed"
                                                                >
                                                                    · {elapsed}
                                                                </span>
                                                            )}
                                                        </>
                                                    ) : (
                                                        "Docker offline"
                                                    )}
                                                </span>
                                            );
                                        })()}
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
                                                <>
                                                    <Button
                                                        size="sm"
                                                        variant="outline-secondary"
                                                        onClick={() => void onViewLogs(purpose)}
                                                        data-testid="backend-view-logs"
                                                        title="Show the last 200 lines of the container's stdout + stderr"
                                                    >
                                                        <i
                                                            className="bi bi-terminal me-1"
                                                            aria-hidden
                                                        />
                                                        Logs
                                                    </Button>
                                                    <Button
                                                        size="sm"
                                                        variant="outline-warning"
                                                        onClick={() => void onRestart(purpose)}
                                                        data-testid="backend-restart"
                                                    >
                                                        Restart
                                                    </Button>
                                                </>
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
                            {/*
                              Live log-line preview for managed rows. Lives
                              on its own row (NOT inside the header
                              `<span>`) so a long line can't push the
                              right-hand button cluster onto a new line.
                              `min-width: 0` is required because flex
                              items default to `min-width: auto`, which
                              means a `nowrap` child won't actually shrink
                              below its intrinsic content width.
                            */}
                            {entry.backend?.mode === "managed" &&
                                statuses[purpose]?.last_log_line && (
                                    <div
                                        className="execlaw-muted small mt-1"
                                        style={{
                                            fontFamily:
                                                'ui-monospace, "SF Mono", Menlo, Consolas, monospace',
                                            fontSize: "0.72rem",
                                            overflow: "hidden",
                                            textOverflow: "ellipsis",
                                            whiteSpace: "nowrap",
                                            minWidth: 0,
                                            maxWidth: "100%",
                                        }}
                                        data-testid="backend-last-log-line"
                                        title={
                                            statuses[purpose].last_log_line ?? ""
                                        }
                                    >
                                        {statuses[purpose].last_log_line}
                                    </div>
                                )}
                            {isEditing && wizardActive && (
                                <div
                                    className="mt-2"
                                    data-testid="backend-wizard-host"
                                >
                                    {!preflightLoaded ? (
                                        <div className="execlaw-muted small">
                                            Probing hardware…
                                        </div>
                                    ) : (
                                        <UnifiedBackendForm
                                            purpose={purpose}
                                            gpus={preflightGpus}
                                            dockerAvailable={preflightDockerOk}
                                            diskFreeBytes={preflightDiskFree}
                                            diskFreePath={preflightDiskPath}
                                            onSubmit={onWizardSubmit}
                                            onSkip={() => setWizardActive(false)}
                                            submitLabel="Save backend"
                                            skipLabel="I'll type the JSON"
                                            testIdPrefix={`backends-wizard-${purpose}`}
                                        />
                                    )}
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
            <HfCacheSection getToken={getToken} canMutate={canMutate} />

            {logsModal && (
                <Modal
                    show
                    onHide={() => setLogsModal(null)}
                    size="lg"
                    scrollable
                    data-testid="backend-logs-modal"
                >
                    <Modal.Header closeButton>
                        <Modal.Title className="h6">
                            {logsModal.purpose} backend logs
                            {logsModal.data?.from_cache && (
                                <span
                                    className="execlaw-trust-badge ms-2 is-blocked"
                                    title="Container is gone; this is the post-mortem tail the supervisor captured before reaping"
                                >
                                    crashed
                                </span>
                            )}
                        </Modal.Title>
                    </Modal.Header>
                    <Modal.Body>
                        {logsModal.loading && (
                            <div
                                className="execlaw-muted small"
                                data-testid="backend-logs-loading"
                            >
                                Loading logs…
                            </div>
                        )}
                        <ErrorBanner
                            message={logsModal.error}
                            onDismiss={() =>
                                setLogsModal((m) =>
                                    m ? { ...m, error: null } : m,
                                )
                            }
                            testId="backend-logs-error"
                        />
                        {logsModal.data &&
                            !logsModal.data.supervisor_available && (
                                <div
                                    className="execlaw-muted small mb-2"
                                    data-testid="backend-logs-supervisor-offline"
                                >
                                    Docker daemon unreachable — the supervisor is
                                    offline so live logs aren&rsquo;t available.
                                </div>
                            )}
                        {logsModal.data &&
                            logsModal.data.logs.trim().length === 0 && (
                                <div
                                    className="execlaw-muted small"
                                    data-testid="backend-logs-empty"
                                >
                                    No log output yet. The container may not
                                    have started, or the supervisor hasn&rsquo;t
                                    captured a tail yet. Try{" "}
                                    <strong>Restart</strong> to kick a fresh
                                    spawn.
                                </div>
                            )}
                        {logsModal.data &&
                            logsModal.data.logs.trim().length > 0 && (
                                <pre
                                    className="execlaw-log-tail mb-0"
                                    data-testid="backend-logs-body"
                                >
                                    {logsModal.data.logs}
                                </pre>
                            )}
                    </Modal.Body>
                    <Modal.Footer>
                        <Button
                            variant="outline-secondary"
                            size="sm"
                            onClick={() => void onViewLogs(logsModal.purpose)}
                            disabled={logsModal.loading}
                            data-testid="backend-logs-refresh"
                        >
                            <i className="bi bi-arrow-clockwise me-1" aria-hidden />
                            Refresh
                        </Button>
                        <Button
                            variant="primary"
                            size="sm"
                            onClick={() => setLogsModal(null)}
                        >
                            Close
                        </Button>
                    </Modal.Footer>
                </Modal>
            )}
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
    const getToken = auth.getAccessToken;
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

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-2" />

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

/// Phase 14.C — operator-supplied additional HuggingFace cache
/// directories. The host-side downloader scans these (read-only)
/// before pulling from huggingface.co; matches get hardlinked or
/// copied into execlaw's primary cache. Useful when the operator
/// already has weights cached from another tool (e.g. their own
/// `~/.cache/huggingface` outside execlaw).
function HfCacheSection({
    getToken,
    canMutate,
}: {
    getToken: () => string | null;
    canMutate: boolean;
}) {
    const [paths, setPaths] = useState<string[] | null>(null);
    const [draft, setDraft] = useState<string>("");
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await getHfCache(getToken);
                if (!cancelled) setPaths(Array.isArray(r?.secondary_paths) ? r.secondary_paths : []);
            } catch (e) {
                // Older server builds (pre-Phase-14.C) don't expose
                // this endpoint and return 404. Treat that as "no
                // secondary caches" rather than blowing up the
                // whole Backends page — the section just renders
                // an empty list and the operator gets the new
                // surface as soon as their server is rebuilt.
                if (!cancelled) {
                    setPaths([]);
                    if (e instanceof Error && !e.message.includes("404")) {
                        setError(e.message);
                    }
                }
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    const save = useCallback(
        async (next: string[]) => {
            setBusy(true);
            setError(null);
            try {
                const r = await putHfCache({ secondary_paths: next }, getToken);
                setPaths(r.secondary_paths);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusy(false);
            }
        },
        [getToken],
    );

    const onAdd = useCallback(() => {
        const trimmed = draft.trim();
        if (!trimmed) return;
        if (paths === null) return;
        if (paths.includes(trimmed)) {
            setError("That path is already in the list.");
            return;
        }
        const next = [...paths, trimmed];
        setDraft("");
        void save(next);
    }, [draft, paths, save]);

    const onRemove = useCallback(
        (p: string) => {
            if (paths === null) return;
            const next = paths.filter((x) => x !== p);
            void save(next);
        },
        [paths, save],
    );

    return (
        <div className="mt-4" data-testid="settings-hf-cache">
            <div className="d-flex align-items-center mb-2">
                <h4 className="h6 mb-0 flex-grow-1">Hugging Face cache</h4>
            </div>
            <p className="execlaw-muted small mb-2">
                Primary cache lives at{" "}
                <code>~/.execlaw/hf-cache/</code>. The host-side
                downloader fetches model weights here once per model;
                the inference container reads from a read-only mount
                of this directory. Add additional cache directories
                below — when a download is requested, execlaw scans
                them first and hardlinks (or copies) matching files
                into the primary cache instead of re-downloading.
                Changes take effect on the next{" "}
                <code>execlaw service restart</code>.
            </p>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-2" />

            {paths === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : (
                <div className="execlaw-card">
                    <div className="execlaw-card__title">
                        Additional caches ({paths.length})
                    </div>
                    {paths.length === 0 && (
                        <div className="execlaw-muted small mb-2">
                            None configured. The downloader uses only the
                            primary cache.
                        </div>
                    )}
                    {paths.map((p) => (
                        <div
                            key={p}
                            className="execlaw-card__row d-flex align-items-center gap-2"
                            data-testid="hf-cache-row"
                        >
                            <code
                                className="flex-grow-1"
                                style={{
                                    overflow: "hidden",
                                    textOverflow: "ellipsis",
                                    whiteSpace: "nowrap",
                                    minWidth: 0,
                                }}
                                title={p}
                            >
                                {p}
                            </code>
                            {canMutate && (
                                <Button
                                    size="sm"
                                    variant="outline-danger"
                                    onClick={() => onRemove(p)}
                                    disabled={busy}
                                    data-testid="hf-cache-remove"
                                >
                                    Remove
                                </Button>
                            )}
                        </div>
                    ))}
                    {canMutate && (
                        <div className="d-flex gap-2 mt-2">
                            <Form.Control
                                size="sm"
                                value={draft}
                                onChange={(e) => setDraft(e.target.value)}
                                placeholder={
                                    'Absolute path, e.g. C:\\Users\\me\\.cache\\huggingface'
                                }
                                disabled={busy}
                                data-testid="hf-cache-draft"
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") {
                                        e.preventDefault();
                                        onAdd();
                                    }
                                }}
                            />
                            <Button
                                size="sm"
                                variant="primary"
                                onClick={onAdd}
                                disabled={busy || draft.trim().length === 0}
                                data-testid="hf-cache-add"
                            >
                                Add
                            </Button>
                        </div>
                    )}
                </div>
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
