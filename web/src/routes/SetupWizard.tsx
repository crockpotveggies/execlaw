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
//        * One or more GPUs → use the shared `UnifiedBackendForm`
//          (single dropdown of detected GPUs + Remote, conditional
//          serving-method radios, memory-filtered model catalog).
//          Same component the Settings → Backends inline wizard
//          uses, so the two surfaces stay in lockstep.
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
    type BackendPurpose,
    type DetectedGpu,
    type PreflightResponse,
    type UpsertBackendRequest,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { useScreenTransition } from "../anim/useScreenTransition";
import {
    UnifiedBackendForm,
    gpuIdString as sharedGpuIdString,
    gpuLabel as sharedGpuLabel,
} from "../settings/UnifiedBackendForm";

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
    // on /setup means `/api/ping` returned `wizard` (or the route
    // guard sent them here from /chat) — skip the account form
    // and start at the docker step.
    //
    // Two reads:
    //   * The `useState` initializer covers the simple case where
    //     auth is already "authenticated" at mount (no flash of
    //     account form on a refresh).
    //   * The `useEffect` covers the post-login race where auth is
    //     briefly "loading" at first render (AuthProvider is still
    //     bootstrapping the /me probe). When it resolves to
    //     authenticated we advance.
    //
    // Without the effect, an operator who logs in with an existing
    // account got stuck staring at the account-creation form,
    // because `useState`'s initializer only runs once and read the
    // pre-resolved auth status.
    const [step, setStep] = useState<WizardStep>(() =>
        auth.status === "authenticated" ? "docker" : "account",
    );
    useEffect(() => {
        if (auth.status === "authenticated" && step === "account") {
            setStep("docker");
        }
    }, [auth.status, step]);
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
    const diskFreeBytes = preflight?.disk_free_bytes ?? null;
    const diskFreePath = preflight?.disk_free_path ?? null;

    // First-run wizard always configures the Standard slot. Settings →
    // Backends uses the same component for all four purposes via a
    // dropdown of its own.
    const handleSubmit = useCallback(
        async (purpose: BackendPurpose, body: UpsertBackendRequest) => {
            await upsertBackend(purpose, body, getToken);
            onComplete();
        },
        [getToken, onComplete],
    );

    return (
        <>
            <p className="execlaw-muted small mb-3">
                Pick where your Standard chat model runs.
            </p>
            <HardwareSummary
                gpus={gpus}
                refreshing={refreshing}
                refresh={refresh}
            />
            <UnifiedBackendForm
                purpose="Standard"
                gpus={gpus}
                dockerAvailable={dockerAvailable}
                diskFreeBytes={diskFreeBytes}
                diskFreePath={diskFreePath}
                onSubmit={handleSubmit}
                onSkip={onSkip}
                testIdPrefix="setup-backend"
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
                                key={sharedGpuIdString(g)}
                                className="execlaw-trust-badge me-1 is-known"
                                data-testid="setup-hardware-gpu"
                            >
                                {sharedGpuLabel(g)}
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
// Unified backend picker — shared with Settings → Backends. See
// `web/src/settings/UnifiedBackendForm.tsx` for the implementation.
// ---------------------------------------------------------------------------
//
// The previous in-file declaration was extracted so both the
// first-run wizard and the inline Backends panel render the same
// target dropdown, serving-method radios, and memory-filtered model
// catalog. Keeping a single source of truth means the Intel Arc fix
// + per-vendor ordinal gpu_id propagate to both surfaces.

