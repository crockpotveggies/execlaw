// First-run setup wizard.
//
// Three-step guided flow (Phase 14):
//
//   1. **Account** — username, password, display name, optional email.
//      POSTs `/api/setup`, then signs the new tokens into AuthContext.
//   2. **Docker** — preflight probe of `docker info`. If missing,
//      surface the Docker Desktop install link + retry button. If
//      reachable (or the operator skips), advance to step 3.
//   3. **Backend** — based on detected GPUs:
//        * No GPU → external OpenAI-compatible endpoint URL form.
//        * One or more GPUs → reuse `BackendWizardPanel` to pick a
//          preset; if multiple GPUs, an inline picker writes the
//          chosen GPU id into the saved row.
//      Saves the Standard backend via `PUT /api/admin/backends/Standard`.
//      Skip is supported on every step — the operator can finish
//      setup later via Settings → Backends.
//
// On final completion (or skip from step 3) the wizard navigates to
// `/chat`, dismissing with the existing scale-down transition. The
// auth context is already in `authenticated` state from step 1, so
// the chat shell has live tokens by the time the navigation fires.

import {
    useCallback,
    useEffect,
    useState,
    type FormEvent,
} from "react";
import { useNavigate } from "react-router-dom";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import { ApiError } from "../api/client";
import {
    dismissSetupWizard,
    getSetupPreflight,
    postSetup,
    upsertBackend,
    type DetectedGpu,
    type PreflightResponse,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { useScreenTransition } from "../anim/useScreenTransition";

interface FieldErrors {
    username?: string;
    display_name?: string;
    admin_password?: string;
    email?: string;
}

const PASSWORD_MIN_LEN = 8;
const USERNAME_MIN_LEN = 3;
const USERNAME_MAX_LEN = 32;
const USERNAME_PATTERN = /^[a-zA-Z0-9_-]+$/;

const DOCKER_DESKTOP_URL = "https://www.docker.com/products/docker-desktop/";

export function validateSetupForm(input: {
    username: string;
    display_name: string;
    admin_password: string;
    email: string;
}): FieldErrors {
    const errors: FieldErrors = {};

    const trimmedUsername = input.username.trim();
    if (trimmedUsername.length === 0) {
        errors.username = "Required.";
    } else if (trimmedUsername.length < USERNAME_MIN_LEN) {
        errors.username = `Must be at least ${USERNAME_MIN_LEN} characters.`;
    } else if (trimmedUsername.length > USERNAME_MAX_LEN) {
        errors.username = `Must be at most ${USERNAME_MAX_LEN} characters.`;
    } else if (!USERNAME_PATTERN.test(trimmedUsername)) {
        errors.username = "Letters, digits, underscore, hyphen only.";
    }

    if (input.display_name.trim().length === 0) {
        errors.display_name = "Required.";
    }
    if (input.admin_password.length < PASSWORD_MIN_LEN) {
        errors.admin_password = `Must be at least ${PASSWORD_MIN_LEN} characters.`;
    }
    if (input.email.trim().length > 0) {
        // Loose RFC-ish check — the backend doesn't validate either,
        // so this just stops obvious typos rather than enforcing a
        // canonical form.
        if (!/^\S+@\S+\.\S+$/.test(input.email.trim())) {
            errors.email = "Doesn't look like an email address.";
        }
    }
    return errors;
}

type WizardStep = "account" | "docker" | "backend";

export function SetupWizard() {
    const auth = useAuth();
    const navigate = useNavigate();
    // Setup card: scale up from 0.85 + fade in on mount, shrink + fade
    // out on successful first-run sign-in.
    const { ref, dismiss } = useScreenTransition<HTMLDivElement>();

    // Phase-14 resume-mid-wizard: an authenticated operator landing
    // on /setup means `/api/ping` returned `wizard` from AppBoot —
    // skip the account form and start at the docker step. The
    // wizard component is mounted only when /api/ping says we
    // should be here, so we don't need to re-probe ping inside.
    const initialStep: WizardStep =
        auth.status === "authenticated" ? "docker" : "account";
    const [step, setStep] = useState<WizardStep>(initialStep);
    // Preflight state lifted to the parent so both Docker and
    // Backend steps share the same source of truth — when the
    // operator clicks "Refresh hardware" on the Backend step it
    // re-runs the same probe + propagates docker availability back
    // (in case dockerd was just started).
    const [preflight, setPreflight] = useState<PreflightResponse | null>(null);
    const [preflightLoading, setPreflightLoading] = useState(false);
    const [preflightError, setPreflightError] = useState<string | null>(null);

    const getToken = useCallback(() => auth.getAccessToken(), [auth]);

    const refreshPreflight = useCallback(async () => {
        setPreflightLoading(true);
        setPreflightError(null);
        try {
            const r = await getSetupPreflight(getToken);
            setPreflight(r);
        } catch (e) {
            setPreflightError(
                e instanceof Error ? e.message : String(e),
            );
        } finally {
            setPreflightLoading(false);
        }
    }, [getToken]);

    const finish = useCallback(() => {
        // Same shrink + fade as the original single-screen wizard.
        // The "complete" path runs after a successful backend save:
        // ping naturally flips to `pong` because the Standard
        // backend now exists, so the chat shell admits the operator.
        dismiss(() => navigate("/chat", { replace: true }));
    }, [dismiss, navigate]);

    const skipBackend = useCallback(async () => {
        // The "Skip for now" path: persist the dismissal so the
        // RequireSetupComplete guard stops bouncing the operator
        // back to /setup on every navigation. Failure is non-fatal
        // — a transient network blip shouldn't trap the operator on
        // the wizard. We log and continue; the next ping check on
        // /chat will simply route them back here.
        try {
            await dismissSetupWizard(getToken);
        } catch (e) {
            console.warn("setup wizard dismiss failed:", e);
        }
        finish();
    }, [getToken, finish]);

    // Note: we deliberately don't bounce authenticated users away
    // from /setup. AppBoot routes here only when ping says `setup`
    // or `wizard`; if the operator lands here directly via URL when
    // ping is `pong`, the wizard's Skip+finish paths still safely
    // route to /chat. The previous "auth-snapshot at mount" guard
    // is gone with the move to ping-driven routing.

    return (
        <div className="execlaw-auth-shell">
            <div ref={ref} className="execlaw-auth-card">
                <h1 className="execlaw-brand h4 mb-1">execlaw</h1>
                <SetupStepIndicator step={step} preflight={preflight} />
                {step === "account" && (
                    <AccountStep
                        onComplete={() => setStep("docker")}
                    />
                )}
                {step === "docker" && (
                    <DockerStep
                        preflight={preflight}
                        loading={preflightLoading}
                        error={preflightError}
                        refresh={refreshPreflight}
                        onContinue={() => setStep("backend")}
                        onSkip={() => setStep("backend")}
                    />
                )}
                {step === "backend" && (
                    <BackendStep
                        getToken={getToken}
                        preflight={preflight}
                        refreshing={preflightLoading}
                        refresh={refreshPreflight}
                        onComplete={finish}
                        onSkip={() => void skipBackend()}
                    />
                )}
            </div>
        </div>
    );
}

/// Step status used to drive the timeline indicator's visuals.
/// "current" — operator is on this step right now.
/// "done"    — moved past it (or, for Docker, preflight succeeded).
/// "upcoming" — not reached yet.
type StepStatus = "upcoming" | "current" | "done";

const STEP_ORDER: WizardStep[] = ["account", "docker", "backend"];
const STEP_LABELS: Record<WizardStep, string> = {
    account: "Account",
    docker: "Docker",
    backend: "Backend",
};

function SetupStepIndicator({
    step,
    preflight,
}: {
    step: WizardStep;
    preflight: PreflightResponse | null;
}) {
    const currentIdx = STEP_ORDER.indexOf(step);
    const dockerOk = preflight?.docker.available === true;

    function statusFor(idx: number, key: WizardStep): StepStatus {
        if (idx < currentIdx) return "done";
        if (idx > currentIdx) return "upcoming";
        // Special-case the Docker step: while we're sitting on it,
        // a successful preflight already proves the prerequisite is
        // satisfied — show the green check immediately rather than
        // making the operator click Continue first. Mirrors the
        // Microsoft-365 example where a completed step gets its
        // check even if the user hasn't moved on yet.
        if (key === "docker" && dockerOk) return "done";
        return "current";
    }

    return (
        <div
            className="execlaw-stepper"
            data-testid="setup-step-indicator"
            role="list"
            aria-label="Setup progress"
        >
            {STEP_ORDER.map((s, i) => {
                const status = statusFor(i, s);
                return (
                    <div
                        key={s}
                        role="listitem"
                        className={
                            "execlaw-stepper__step" +
                            (status === "current" ? " is-current" : "") +
                            (status === "done" ? " is-done" : "")
                        }
                        data-testid={`setup-step-${s}`}
                        data-status={status}
                        aria-current={status === "current" ? "step" : undefined}
                    >
                        <div
                            className="execlaw-stepper__circle"
                            aria-hidden
                        >
                            {status === "done" ? (
                                <i className="bi bi-check-lg" />
                            ) : (
                                <span>{i + 1}</span>
                            )}
                        </div>
                        <div className="execlaw-stepper__label">
                            {STEP_LABELS[s]}
                        </div>
                    </div>
                );
            })}
        </div>
    );
}

// ---------------------------------------------------------------------------
// Step 1 — Account
// ---------------------------------------------------------------------------

function AccountStep({ onComplete }: { onComplete: () => void }) {
    const auth = useAuth();
    const [username, setUsername] = useState("");
    const [displayName, setDisplayName] = useState("");
    const [password, setPassword] = useState("");
    const [email, setEmail] = useState("");
    const [errors, setErrors] = useState<FieldErrors>({});
    const [submitError, setSubmitError] = useState<string | null>(null);
    const [submitting, setSubmitting] = useState(false);
    const navigate = useNavigate();

    const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
        e.preventDefault();
        setSubmitError(null);
        const fieldErrs = validateSetupForm({
            username,
            display_name: displayName,
            admin_password: password,
            email,
        });
        setErrors(fieldErrs);
        if (Object.keys(fieldErrs).length > 0) return;

        setSubmitting(true);
        try {
            const trimmedEmail = email.trim();
            const resp = await postSetup({
                username: username.trim(),
                admin_password: password,
                display_name: displayName.trim(),
                ...(trimmedEmail.length > 0 ? { email: trimmedEmail } : {}),
            });
            await auth.signIn({
                access_token: resp.access_token,
                refresh_token: resp.refresh_token,
            });
            onComplete();
            return;
        } catch (e) {
            if (e instanceof ApiError && e.serverCode === "already_initialized") {
                navigate("/login", { replace: true });
                return;
            }
            setSubmitError(
                e instanceof Error ? e.message : "Setup failed; try again.",
            );
        } finally {
            setSubmitting(false);
        }
    };

    return (
        <>
            <p className="execlaw-muted small mb-4">
                Welcome — let&rsquo;s create your controller account.
            </p>

            {submitError && (
                <div
                    className="execlaw-error-banner mb-3"
                    role="alert"
                    data-testid="setup-submit-error"
                >
                    {submitError}
                </div>
            )}

            <Form noValidate onSubmit={onSubmit} data-testid="setup-account-form">
                <Form.Group className="mb-3" controlId="setup-username">
                    <Form.Label>Username</Form.Label>
                    <Form.Control
                        type="text"
                        autoComplete="username"
                        value={username}
                        onChange={(e) => setUsername(e.target.value)}
                        isInvalid={!!errors.username}
                        disabled={submitting}
                        autoFocus
                        spellCheck={false}
                        autoCapitalize="none"
                    />
                    <Form.Control.Feedback type="invalid">
                        {errors.username}
                    </Form.Control.Feedback>
                    <Form.Text className="execlaw-muted">
                        Used to sign in. Letters, digits, underscore, hyphen.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-3" controlId="setup-display-name">
                    <Form.Label>Display name</Form.Label>
                    <Form.Control
                        type="text"
                        autoComplete="name"
                        value={displayName}
                        onChange={(e) => setDisplayName(e.target.value)}
                        isInvalid={!!errors.display_name}
                        disabled={submitting}
                    />
                    <Form.Control.Feedback type="invalid">
                        {errors.display_name}
                    </Form.Control.Feedback>
                </Form.Group>

                <Form.Group className="mb-3" controlId="setup-password">
                    <Form.Label>Admin password</Form.Label>
                    <Form.Control
                        type="password"
                        autoComplete="new-password"
                        value={password}
                        onChange={(e) => setPassword(e.target.value)}
                        isInvalid={!!errors.admin_password}
                        disabled={submitting}
                    />
                    <Form.Control.Feedback type="invalid">
                        {errors.admin_password}
                    </Form.Control.Feedback>
                    <Form.Text className="execlaw-muted">
                        At least {PASSWORD_MIN_LEN} characters. You can change it later.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-4" controlId="setup-email">
                    <Form.Label>
                        Email <span className="execlaw-muted">(optional)</span>
                    </Form.Label>
                    <Form.Control
                        type="email"
                        autoComplete="email"
                        value={email}
                        onChange={(e) => setEmail(e.target.value)}
                        isInvalid={!!errors.email}
                        disabled={submitting}
                    />
                    <Form.Control.Feedback type="invalid">
                        {errors.email}
                    </Form.Control.Feedback>
                </Form.Group>

                <Button
                    type="submit"
                    variant="primary"
                    className="w-100"
                    disabled={submitting}
                    data-testid="setup-account-submit"
                >
                    {submitting ? (
                        <>
                            <Spinner size="sm" animation="border" className="me-2" />
                            Creating…
                        </>
                    ) : (
                        "Create account"
                    )}
                </Button>
            </Form>
        </>
    );
}

// ---------------------------------------------------------------------------
// Step 2 — Docker
// ---------------------------------------------------------------------------

function DockerStep({
    preflight,
    loading,
    error,
    refresh,
    onContinue,
    onSkip,
}: {
    preflight: PreflightResponse | null;
    loading: boolean;
    error: string | null;
    refresh: () => Promise<void>;
    onContinue: () => void;
    onSkip: () => void;
}) {
    // Mount → kick the first probe. Lifted preflight means we don't
    // re-fetch on re-render, just the initial mount and explicit
    // user-initiated refreshes.
    useEffect(() => {
        if (preflight === null && !loading && error === null) {
            void refresh();
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    if (loading && preflight === null) {
        return (
            <>
                <p className="execlaw-muted small mb-3">
                    Checking for Docker…
                </p>
                <div data-testid="setup-docker-loading">
                    <Spinner size="sm" animation="border" className="me-2" />
                    Probing the Docker daemon…
                </div>
            </>
        );
    }

    if (error !== null && preflight === null) {
        return (
            <>
                <p className="execlaw-muted small mb-3">
                    Couldn&rsquo;t reach the preflight endpoint.
                </p>
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
                <div className="d-flex gap-2">
                    <Button
                        variant="primary"
                        onClick={() => void refresh()}
                        disabled={loading}
                        data-testid="setup-docker-retry"
                    >
                        Retry
                    </Button>
                    <Button
                        variant="outline-secondary"
                        onClick={onSkip}
                        data-testid="setup-docker-skip"
                    >
                        Skip
                    </Button>
                </div>
            </>
        );
    }

    const docker = preflight?.docker ?? { available: false, version: null };

    if (docker.available) {
        return (
            <div data-testid="setup-docker-ok">
                <p className="execlaw-muted small mb-3">
                    Looking for prerequisites…
                </p>
                <div className="execlaw-card mb-3">
                    <div className="execlaw-card__title d-flex align-items-center">
                        <i
                            className="bi bi-check-circle-fill text-success me-2"
                            aria-hidden
                        />
                        Docker is reachable
                    </div>
                    {docker.version && (
                        <div className="execlaw-muted small">
                            Server version: <code>{docker.version}</code>
                        </div>
                    )}
                </div>
                <div className="d-flex gap-2">
                    <Button
                        variant="primary"
                        onClick={onContinue}
                        data-testid="setup-docker-continue"
                    >
                        Continue
                    </Button>
                    <Button
                        variant="outline-secondary"
                        onClick={() => void refresh()}
                        disabled={loading}
                        data-testid="setup-docker-recheck"
                    >
                        {loading ? (
                            <>
                                <Spinner
                                    size="sm"
                                    animation="border"
                                    className="me-2"
                                />
                                Re-checking…
                            </>
                        ) : (
                            "Re-check"
                        )}
                    </Button>
                </div>
            </div>
        );
    }

    return (
        <div data-testid="setup-docker-missing">
            <p className="execlaw-muted small mb-3">
                Docker is needed for managed inference backends.
            </p>
            <div className="execlaw-card mb-3">
                <div className="execlaw-card__title d-flex align-items-center">
                    <i
                        className="bi bi-exclamation-triangle-fill text-warning me-2"
                        aria-hidden
                    />
                    Docker Desktop not detected
                </div>
                <div className="execlaw-muted small">
                    execlaw uses Docker to spawn the inference containers
                    that run Whisper, Kokoro, and your LLM. Without it
                    you can still point execlaw at an external
                    OpenAI-compatible endpoint, but the in-app backend
                    wizard won&rsquo;t be able to manage containers for
                    you.
                </div>
                <div className="mt-2">
                    <a
                        href={DOCKER_DESKTOP_URL}
                        target="_blank"
                        rel="noreferrer noopener"
                        className="btn btn-outline-primary btn-sm"
                        data-testid="setup-docker-install-link"
                    >
                        <i
                            className="bi bi-box-arrow-up-right me-1"
                            aria-hidden
                        />
                        Install Docker Desktop
                    </a>
                </div>
            </div>
            <div className="d-flex gap-2">
                <Button
                    variant="primary"
                    onClick={() => void refresh()}
                    disabled={loading}
                    data-testid="setup-docker-retry"
                >
                    {loading ? (
                        <>
                            <Spinner
                                size="sm"
                                animation="border"
                                className="me-2"
                            />
                            Re-checking…
                        </>
                    ) : (
                        "I've installed it, re-check"
                    )}
                </Button>
                <Button
                    variant="outline-secondary"
                    onClick={onSkip}
                    data-testid="setup-docker-skip"
                >
                    Skip for now
                </Button>
            </div>
        </div>
    );
}

// ---------------------------------------------------------------------------
// Step 3 — Backend
// ---------------------------------------------------------------------------

function BackendStep({
    getToken,
    preflight,
    refreshing,
    refresh,
    onComplete,
    onSkip,
}: {
    getToken: () => string | null;
    preflight: PreflightResponse | null;
    refreshing: boolean;
    refresh: () => Promise<void>;
    onComplete: () => void;
    onSkip: () => void;
}) {
    const gpus = preflight?.gpus ?? [];
    const dockerAvailable = preflight?.docker.available ?? false;
    return (
        <>
            <HardwareSummary
                gpus={gpus}
                refreshing={refreshing}
                refresh={refresh}
            />
            <UnifiedBackendForm
                getToken={getToken}
                gpus={gpus}
                dockerAvailable={dockerAvailable}
                onComplete={onComplete}
                onSkip={onSkip}
            />
        </>
    );
}

/// Top-of-step hardware summary with a re-probe button so the
/// operator can plug in / fix drivers and re-detect without leaving
/// the wizard. Visible regardless of whether the managed path or
/// the external-URL path is currently rendered below it.
function HardwareSummary({
    gpus,
    refreshing,
    refresh,
}: {
    gpus: DetectedGpu[];
    refreshing: boolean;
    refresh: () => Promise<void>;
}) {
    const usable = gpus.filter(
        (g) => g.vendor === "Nvidia" || g.vendor === "Intel" || g.vendor === "Amd",
    );
    return (
        <div
            className="execlaw-card mb-3 d-flex align-items-start gap-2"
            data-testid="setup-hardware-summary"
        >
            <div className="flex-grow-1">
                <div className="execlaw-card__title">Detected hardware</div>
                {usable.length === 0 ? (
                    <div className="execlaw-muted small">
                        No supported GPU detected. Plug one in (or fix
                        drivers) and click Refresh, or skip and use an
                        external endpoint below.
                    </div>
                ) : (
                    <div className="execlaw-muted small">
                        {usable.map((g, i) => (
                            <span
                                key={gpuIdString(g)}
                                className="execlaw-trust-badge me-1 is-known"
                                data-testid="setup-hardware-gpu"
                            >
                                {gpuLabel(g)}
                                {i < usable.length - 1 ? "" : ""}
                            </span>
                        ))}
                    </div>
                )}
            </div>
            <Button
                variant="outline-secondary"
                size="sm"
                onClick={() => void refresh()}
                disabled={refreshing}
                data-testid="setup-hardware-refresh"
            >
                {refreshing ? (
                    <>
                        <Spinner
                            size="sm"
                            animation="border"
                            className="me-2"
                        />
                        Refreshing…
                    </>
                ) : (
                    <>
                        <i
                            className="bi bi-arrow-clockwise me-1"
                            aria-hidden
                        />
                        Refresh
                    </>
                )}
            </Button>
        </div>
    );
}

// ---------------------------------------------------------------------------
// Unified backend picker
//
// Single dropdown: "target" — every detected GPU plus a "Remote
// OpenAI-compatible endpoint" sentinel. Below the target picker the
// form switches:
//
//   * GPU + NVIDIA  → vLLM is the only serving method (locked-in
//                     because that's what the production presets
//                     point at; the operator doesn't pick).
//   * GPU + Intel   → radios for OpenVINO vs OpenArc.
//   * GPU + AMD     → not yet supported; the wizard hides AMD GPUs
//                     from the dropdown so the operator routes
//                     through Remote until ROCm/vLLM-AMD lands.
//   * Remote        → URL + optional model-id form.
//
// On GPU targets a model dropdown surfaces below the serving
// method, filtered to entries that fit in the chosen card's VRAM.
// ---------------------------------------------------------------------------

type ServingMethod = "vllm" | "openvino" | "openarc";

interface ModelOption {
    /// Hugging Face repo id / OpenVINO model id — used as the
    /// `--model={id}` CLI arg the supervisor passes to the chosen
    /// container.
    id: string;
    /// Display label in the dropdown.
    label: string;
    /// Approximate minimum VRAM required, in MiB. The wizard hides
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

const SERVING_IMAGE: Record<ServingMethod, string> = {
    vllm: "vllm/vllm-openai:v0.6.2",
    openvino: "execlaw/service-vllm-openvino-arc:v1",
    openarc: "execlaw/service-openarc:v1",
};

/// Curated catalog. Sized to fit the preset library's locked
/// decisions (Qwen3.5-27B-AWQ as the flagship Standard model, plus
/// smaller fallbacks). The user can override anything via the
/// Settings → Backends page once setup completes.
const MODEL_CATALOG: Record<ServingMethod, ModelOption[]> = {
    vllm: [
        {
            id: "QuantTrio/Qwen3.5-27B-AWQ",
            label: "Qwen 3.5 27B (AWQ, ~18 GB)",
            min_mb: 18_000,
        },
        {
            id: "Qwen/Qwen2.5-7B-Instruct-AWQ",
            label: "Qwen 2.5 7B (AWQ, ~8 GB)",
            min_mb: 8_000,
        },
        {
            id: "Qwen/Qwen2.5-3B-Instruct-AWQ",
            label: "Qwen 2.5 3B (AWQ, ~4 GB)",
            min_mb: 4_000,
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
    /// Index in the gpu list — used as a stable React key.
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
    if (memory_mb === null) {
        // Unknown VRAM — show everything; operator can override
        // post-setup if it doesn't fit.
        return all;
    }
    return all.filter((m) => m.min_mb <= memory_mb);
}

function UnifiedBackendForm({
    getToken,
    gpus,
    dockerAvailable,
    onComplete,
    onSkip,
}: {
    getToken: () => string | null;
    gpus: DetectedGpu[];
    dockerAvailable: boolean;
    onComplete: () => void;
    onSkip: () => void;
}) {
    // Build the target list. We include a GPU only if (a) Docker is
    // available AND (b) the vendor has at least one supported serving
    // method (NVIDIA / Intel Arc). AMD + Apple Silicon + Unknown
    // route through Remote until their plugins ship.
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

    // Default the serving method to the first available for the
    // currently-selected GPU. Resets when the operator switches
    // GPUs.
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

    // Available models for the current GPU + serving combo.
    const availableModels =
        target.kind === "gpu"
            ? modelsFor(serving, target.gpu.memory_mb ?? null)
            : [];
    const [modelId, setModelId] = useState<string>("");
    // Reset model when target / serving changes — picks the first
    // available so the dropdown is never empty.
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

    // Remote-mode fields.
    const [endpoint, setEndpoint] = useState("");
    const [remoteModel, setRemoteModel] = useState("");

    // Submission state.
    const [submitting, setSubmitting] = useState(false);
    const [submitError, setSubmitError] = useState<string | null>(null);
    const [endpointError, setEndpointError] = useState<string | null>(null);

    const onSubmit = useCallback(
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
                    await upsertBackend(
                        "Standard",
                        {
                            inference_backend: "external",
                            model_spec: remoteModel.trim().length > 0
                                ? { model: remoteModel.trim() }
                                : {},
                            gpu_id: null,
                            endpoint: trimmed,
                            notes: "Configured via first-run wizard (remote)",
                            reasoning_enabled: false,
                            mode: "external",
                        },
                        getToken,
                    );
                } else {
                    if (!modelId) {
                        setSubmitError(
                            "Pick a model that fits this GPU (or skip and configure later).",
                        );
                        setSubmitting(false);
                        return;
                    }
                    await upsertBackend(
                        "Standard",
                        {
                            inference_backend: SERVING_PLUGIN[serving],
                            model_spec: {
                                image: SERVING_IMAGE[serving],
                                args: [`--model=${modelId}`],
                                container_port: 8000,
                            },
                            gpu_id: gpuIdString(target.gpu),
                            endpoint: null,
                            notes: `Configured via first-run wizard (${SERVING_LABEL[serving]})`,
                            reasoning_enabled: false,
                            mode: "managed",
                        },
                        getToken,
                    );
                }
                onComplete();
            } catch (err) {
                setSubmitError(
                    err instanceof Error ? err.message : String(err),
                );
            } finally {
                setSubmitting(false);
            }
        },
        [
            target,
            serving,
            modelId,
            endpoint,
            remoteModel,
            getToken,
            onComplete,
        ],
    );

    return (
        <div data-testid="setup-backend-unified">
            <p className="execlaw-muted small mb-3">
                Pick where your Standard chat model runs.
            </p>
            {submitError && (
                <div
                    className="execlaw-error-banner mb-3"
                    role="alert"
                    data-testid="setup-backend-error"
                >
                    {submitError}
                </div>
            )}
            <Form noValidate onSubmit={onSubmit}>
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
                        data-testid="setup-target-select"
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
                            plane only speaks
                            {" "}<code>POST /v1/chat/completions</code>{" "}
                            (no vendor SDKs).
                        </Form.Text>
                    )}
                </Form.Group>

                {target.kind === "gpu" && (
                    <>
                        {availableServing.length === 1 ? (
                            <div
                                className="execlaw-muted small mb-3"
                                data-testid="setup-serving-fixed"
                            >
                                Serving method:{" "}
                                <strong>{SERVING_LABEL[serving]}</strong>
                                {" "}(only supported method for this GPU
                                vendor today).
                            </div>
                        ) : (
                            <Form.Group
                                className="mb-3"
                                data-testid="setup-serving-picker"
                            >
                                <Form.Label className="execlaw-muted small mb-1">
                                    Serving method
                                </Form.Label>
                                <div className="d-flex gap-3">
                                    {availableServing.map((m) => (
                                        <Form.Check
                                            key={m}
                                            type="radio"
                                            id={`setup-serving-${m}`}
                                            name="setup-serving"
                                            label={SERVING_LABEL[m]}
                                            checked={serving === m}
                                            onChange={() => setServing(m)}
                                            disabled={submitting}
                                            data-testid={`setup-serving-${m}`}
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
                                    data-testid="setup-no-models"
                                >
                                    No model in the curated catalog fits
                                    this GPU&rsquo;s {target.gpu.memory_mb ?? "?"}
                                    {" "}MiB of VRAM. Skip and configure a
                                    custom model later via Settings →
                                    Backends.
                                </div>
                            ) : (
                                <>
                                    <Form.Select
                                        value={modelId}
                                        onChange={(e) => setModelId(e.target.value)}
                                        disabled={submitting}
                                        data-testid="setup-model-select"
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
                                        . You can swap models later from
                                        Settings → Backends.
                                    </Form.Text>
                                </>
                            )}
                        </Form.Group>
                    </>
                )}

                {target.kind === "remote" && (
                    <>
                        <Form.Group className="mb-3" controlId="setup-external-endpoint">
                            <Form.Label>Endpoint URL</Form.Label>
                            <Form.Control
                                type="url"
                                value={endpoint}
                                onChange={(e) => setEndpoint(e.target.value)}
                                isInvalid={!!endpointError}
                                disabled={submitting}
                                placeholder="http://localhost:8000/v1"
                                data-testid="setup-external-endpoint"
                            />
                            <Form.Control.Feedback type="invalid">
                                {endpointError}
                            </Form.Control.Feedback>
                            <Form.Text className="execlaw-muted">
                                Include the <code>/v1</code> suffix if
                                your server requires it.
                            </Form.Text>
                        </Form.Group>
                        <Form.Group className="mb-3" controlId="setup-external-model">
                            <Form.Label>
                                Model id <span className="execlaw-muted">(optional)</span>
                            </Form.Label>
                            <Form.Control
                                type="text"
                                value={remoteModel}
                                onChange={(e) => setRemoteModel(e.target.value)}
                                disabled={submitting}
                                placeholder="QuantTrio/Qwen3.5-27B-AWQ"
                                data-testid="setup-external-model"
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
                        data-testid="setup-backend-submit"
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
                            "Save backend"
                        )}
                    </Button>
                    <Button
                        type="button"
                        variant="outline-secondary"
                        onClick={onSkip}
                        disabled={submitting}
                        data-testid="setup-backend-skip"
                    >
                        Skip for now
                    </Button>
                </div>
            </Form>
        </div>
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function gpuLabel(g: DetectedGpu): string {
    const vendor = vendorDisplayName(g.vendor);
    // Prefer the resolved SKU (e.g. "GeForce RTX 4090") + memory if
    // hardware-query gave us those. Falls back to a short
    // vendor + cleaned PCI device id when only the legacy sysfs
    // path is available — never the multi-line PNP string.
    const sku =
        g.model_name && g.model_name.trim().length > 0
            ? g.model_name.trim()
            : `${vendor} GPU (${cleanPciDeviceId(g.pci_device_id)})`;
    const mem = g.memory_mb && g.memory_mb > 0
        ? ` · ${(g.memory_mb / 1024).toFixed(1)} GB`
        : "";
    // For NVIDIA the SKU usually already includes "NVIDIA" or
    // "GeForce" so prefixing the vendor would be redundant. For
    // Intel Arc the SKU is just "Arc A770", so prefix with "Intel".
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

/// Strip multi-line / over-long PCI device strings down to a short
/// label suitable for a badge. Defends against the Windows PNP
/// shape (`PCI\VEN_…&DEV_NNNN&…`) that the server-side adapter is
/// supposed to clean up — we mirror the heuristic so older servers
/// that pre-date the cleanup still render correctly.
function cleanPciDeviceId(raw: string): string {
    if (raw.startsWith("0x") || raw.startsWith("0X")) return raw;
    const devIdx = raw.indexOf("DEV_");
    if (devIdx >= 0) {
        const hex = raw.slice(devIdx + 4, devIdx + 8);
        if (/^[0-9a-fA-F]{4}$/.test(hex)) return `0x${hex.toLowerCase()}`;
    }
    return raw.length > 14 ? `${raw.slice(0, 13)}…` : raw;
}

function gpuIdString(g: DetectedGpu): string {
    // The server sends `GpuId` as either a tuple struct or a plain
    // string depending on serde derives. Handle both.
    if (typeof g.id === "string") return g.id;
    if (g.id && typeof g.id === "object" && "0" in g.id) {
        return String(g.id[0]);
    }
    return `${g.pci_vendor_id}:${g.pci_device_id}`;
}
