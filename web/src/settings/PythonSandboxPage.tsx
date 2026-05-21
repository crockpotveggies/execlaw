// Settings → Python sandbox.
//
// 2026-05-20 — native feature config page. Replaces the previous
// plugin's config panel (formerly mounted at
// `/settings/plugins/python-sandbox` via DynamicPluginPanel).
// Python-sandbox was migrated from a plugin to a native feature
// because the implementation was tightly coupled to the host
// crate (5,685 lines of Rust for kernel pool, Jupyter WS protocol,
// output watcher, etc.) and the plugin SDK doesn't support
// shipping Rust modules — pretending it was a plugin violated
// the encapsulation grounding rule.
//
// What this page does:
//   * Top-level enable/disable toggle. When off, the host skips
//     sidecar registration AND tool registration entirely — the
//     `python.execute` family disappears from the agent's catalog.
//   * Tunable settings (idle timeout, max output bytes). Match
//     the boundaries the wiring layer enforces server-side.
//   * Sidecar status block (only when enabled) — polls every 3s,
//     shows the same Healthy / Starting / CrashLooping chip as the
//     Sidecars admin page.
//   * Docker-availability indicator. When Docker isn't detected
//     (common on Apple Silicon without Docker Desktop), the
//     enable toggle is disabled and a hint explains why. The boot
//     path also auto-disables in this case, so the toggle flipping
//     off on a fresh boot reflects this.
//
// Persistence: PUT to /api/admin/python-sandbox writes the config
// row. Changes take effect on the next server restart — same
// "applies on next restart" convention as Settings → General's
// bind-address field.

import { useCallback, useEffect, useRef, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    getPythonSandbox,
    updatePythonSandbox,
    type PythonSandboxStatusResponse,
    type SidecarStatusView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

/// Bounds match the wiring + admin-route validation server-side.
/// A value outside these gets rejected with a clear API error;
/// client-side guard prevents the submit in the first place.
const IDLE_TIMEOUT_MIN_SECS = 60;
const IDLE_TIMEOUT_MAX_SECS = 24 * 60 * 60;
const MAX_OUTPUT_MIN_BYTES = 1024 * 1024;
const MAX_OUTPUT_MAX_BYTES = 500 * 1024 * 1024;

const DEFAULT_IDLE_TIMEOUT = 900;
const DEFAULT_MAX_OUTPUT = 50 * 1024 * 1024;

/// Sidecar poll cadence while the operator is on-page. 3s matches
/// the convention the SidecarsPage uses; responsive enough that a
/// recently-flipped enable surfaces the spawn within one tick.
const POLL_INTERVAL_MS = 3_000;

export function PythonSandboxPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [snapshot, setSnapshot] = useState<PythonSandboxStatusResponse | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [saving, setSaving] = useState(false);
    const [saved, setSaved] = useState(false);

    /// Form state — operator's pending edits before Save. Initialized
    /// from the loaded config row; reset on a fresh load.
    const [enabled, setEnabled] = useState(false);
    const [idleTimeout, setIdleTimeout] = useState<number>(DEFAULT_IDLE_TIMEOUT);
    const [maxOutput, setMaxOutput] = useState<number>(DEFAULT_MAX_OUTPUT);
    const [dirty, setDirty] = useState(false);

    /// We re-fetch every POLL_INTERVAL_MS to pick up live sidecar
    /// status, but only stamp the form on the FIRST load (or after
    /// a successful save). Without this, the operator's mid-edit
    /// values get clobbered by every poll tick.
    const loadedOnce = useRef(false);

    const refresh = useCallback(
        async (resetForm: boolean) => {
            try {
                const r = await getPythonSandbox(getToken);
                setSnapshot(r);
                if (resetForm || !loadedOnce.current) {
                    setEnabled(r.config.enabled);
                    setIdleTimeout(r.config.idle_timeout_seconds);
                    setMaxOutput(r.config.max_output_bytes);
                    setDirty(false);
                    loadedOnce.current = true;
                }
                setError(null);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken],
    );

    useEffect(() => {
        void refresh(true);
        const id = window.setInterval(() => {
            void refresh(false);
        }, POLL_INTERVAL_MS);
        return () => window.clearInterval(id);
    }, [refresh]);

    const meRole = auth.user?.role ?? "viewer";
    const canMutate = meRole === "controller";

    const dockerAvailable = snapshot?.docker_available ?? false;
    /// The enable toggle is interactive when:
    ///   * the page has loaded its first snapshot
    ///   * Docker is available (otherwise no sidecar can spawn)
    ///   * the caller is a Controller
    const enableToggleAllowed = snapshot !== null && dockerAvailable && canMutate;

    const onSave = useCallback(async () => {
        if (!snapshot) return;
        // Client-side bounds — the API enforces these too, but
        // catching client-side gives an immediate error without
        // a round-trip.
        if (idleTimeout < IDLE_TIMEOUT_MIN_SECS || idleTimeout > IDLE_TIMEOUT_MAX_SECS) {
            setError(
                `Idle timeout must be ${IDLE_TIMEOUT_MIN_SECS}–${IDLE_TIMEOUT_MAX_SECS} seconds.`,
            );
            return;
        }
        if (maxOutput < MAX_OUTPUT_MIN_BYTES || maxOutput > MAX_OUTPUT_MAX_BYTES) {
            setError(
                `Max output must be ${MAX_OUTPUT_MIN_BYTES}–${MAX_OUTPUT_MAX_BYTES} bytes.`,
            );
            return;
        }
        setSaving(true);
        setError(null);
        setSaved(false);
        try {
            const body: {
                enabled?: boolean;
                idle_timeout_seconds?: number;
                max_output_bytes?: number;
            } = {};
            if (snapshot.config.enabled !== enabled) body.enabled = enabled;
            if (snapshot.config.idle_timeout_seconds !== idleTimeout) {
                body.idle_timeout_seconds = idleTimeout;
            }
            if (snapshot.config.max_output_bytes !== maxOutput) {
                body.max_output_bytes = maxOutput;
            }
            await updatePythonSandbox(body, getToken);
            setSaved(true);
            window.setTimeout(() => setSaved(false), 4_000);
            await refresh(true);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setSaving(false);
        }
    }, [snapshot, enabled, idleTimeout, maxOutput, getToken, refresh]);

    return (
        <div className="execlaw-settings__pane" data-testid="python-sandbox-page">
            <header className="execlaw-settings__pane-head">
                <h2 className="h5 mb-1">
                    <i className="bi bi-filetype-py me-2" aria-hidden />
                    Python sandbox
                </h2>
                <p className="text-body-secondary small mb-0">
                    Persistent per-conversation Python execution sandbox.
                    When enabled, the agent gets four tools
                    (<code>python.execute</code>, <code>python.reset</code>,{" "}
                    <code>python.interrupt</code>, <code>python.list_files</code>)
                    backed by a Jupyter kernel running inside a Docker sidecar.
                </p>
            </header>

            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            {snapshot === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : (
                <>
                    {/* --- Enable toggle --------------------------- */}
                    <section className="mb-4">
                        <Form.Check
                            type="switch"
                            id="python-sandbox-enabled"
                            label={
                                <>
                                    <strong>Enable Python sandbox</strong>
                                    <div className="text-body-secondary small">
                                        Registers the python.* tools + spawns the
                                        kernel-gateway sidecar container at boot.
                                        Changes take effect on next server restart.
                                    </div>
                                </>
                            }
                            checked={enabled}
                            disabled={!enableToggleAllowed || saving}
                            onChange={(e) => {
                                setEnabled(e.target.checked);
                                setDirty(true);
                            }}
                            data-testid="python-sandbox-enable-toggle"
                        />
                        {!dockerAvailable && (
                            <div
                                className="alert alert-warning small mt-2 mb-0"
                                data-testid="python-sandbox-no-docker"
                            >
                                <i
                                    className="bi bi-exclamation-triangle me-2"
                                    aria-hidden
                                />
                                Docker is not detected on this host. The Python
                                sandbox can't spawn its sidecar without Docker.
                                Install Docker Desktop (or equivalent) and
                                restart execlaw to enable. On Apple Silicon,
                                Docker Desktop is the simplest option.
                            </div>
                        )}
                    </section>

                    {/* --- Sidecar status (only when enabled) ----- */}
                    {snapshot.config.enabled && snapshot.sidecar !== null && (
                        <section className="mb-4" data-testid="python-sandbox-sidecar-status">
                            <h3 className="h6 mb-2">Kernel gateway</h3>
                            <SidecarChip sidecar={snapshot.sidecar} />
                        </section>
                    )}
                    {snapshot.config.enabled && snapshot.sidecar === null && dockerAvailable && (
                        <section className="mb-4">
                            <h3 className="h6 mb-2">Kernel gateway</h3>
                            <div className="execlaw-muted small">
                                Sidecar registering… (may take a few seconds
                                after enabling)
                            </div>
                        </section>
                    )}

                    {/* --- Tunables ------------------------------ */}
                    <section className="mb-4">
                        <h3 className="h6 mb-2">Settings</h3>
                        <p className="text-body-secondary small mb-3">
                            Changes take effect on the next server restart.
                        </p>
                        <Form.Group className="mb-3">
                            <Form.Label
                                htmlFor="ps-idle-timeout"
                                className="small fw-semibold"
                            >
                                Kernel idle timeout (seconds)
                            </Form.Label>
                            <Form.Control
                                id="ps-idle-timeout"
                                type="number"
                                size="sm"
                                value={idleTimeout}
                                min={IDLE_TIMEOUT_MIN_SECS}
                                max={IDLE_TIMEOUT_MAX_SECS}
                                step={1}
                                disabled={!canMutate || saving}
                                onChange={(e) => {
                                    const n = Number.parseInt(e.target.value, 10);
                                    if (Number.isFinite(n)) {
                                        setIdleTimeout(n);
                                        setDirty(true);
                                    }
                                }}
                                data-testid="python-sandbox-idle-timeout"
                            />
                            <Form.Text muted className="small">
                                How long an inactive kernel stays alive before
                                the pool evicts it. Default: {DEFAULT_IDLE_TIMEOUT}
                                s (15 min). Lower = less idle memory, more
                                cold starts.
                            </Form.Text>
                        </Form.Group>
                        <Form.Group className="mb-3">
                            <Form.Label
                                htmlFor="ps-max-output"
                                className="small fw-semibold"
                            >
                                Max output bytes per execute
                            </Form.Label>
                            <Form.Control
                                id="ps-max-output"
                                type="number"
                                size="sm"
                                value={maxOutput}
                                min={MAX_OUTPUT_MIN_BYTES}
                                max={MAX_OUTPUT_MAX_BYTES}
                                step={1024 * 1024}
                                disabled={!canMutate || saving}
                                onChange={(e) => {
                                    const n = Number.parseInt(e.target.value, 10);
                                    if (Number.isFinite(n)) {
                                        setMaxOutput(n);
                                        setDirty(true);
                                    }
                                }}
                                data-testid="python-sandbox-max-output"
                            />
                            <Form.Text muted className="small">
                                Hard cap on a single python.execute output.
                                Default: {(DEFAULT_MAX_OUTPUT / 1024 / 1024).toFixed(0)}{" "}
                                MB. Exceeding aborts the kernel and returns{" "}
                                <code>status: output_too_large</code>.
                            </Form.Text>
                        </Form.Group>
                        <div className="d-flex align-items-center gap-2">
                            <Button
                                variant="primary"
                                size="sm"
                                disabled={!canMutate || !dirty || saving}
                                onClick={() => void onSave()}
                                data-testid="python-sandbox-save"
                            >
                                {saving ? "Saving…" : "Save"}
                            </Button>
                            {saved && (
                                <span
                                    className="text-success small"
                                    data-testid="python-sandbox-saved-hint"
                                >
                                    ✓ Saved — applies on next restart
                                </span>
                            )}
                        </div>
                    </section>

                    {/* --- About -------------------------------- */}
                    <section>
                        <h3 className="h6 mb-2">About</h3>
                        <p className="small text-body-secondary mb-2">
                            Pre-installed in the sandbox: pandas, polars, duckdb,
                            pyarrow, numpy, openpyxl, ipython, httpx. Cannot pip
                            install additional packages.
                        </p>
                        <ul className="small text-body-secondary mb-0">
                            <li>
                                <code>/work/uploads/</code> — files the operator
                                attached to this conversation; read-only inside
                                the kernel.
                            </li>
                            <li>
                                <code>/work/outputs/</code> — agent writes files
                                here, host auto-publishes them as conversation
                                artifacts.
                            </li>
                            <li>
                                Kernel state (variables, dataframes, imports)
                                survives across turns until the idle timeout
                                above expires.
                            </li>
                            <li>
                                Charts: agent computes data with pandas / polars
                                / duckdb, then calls <code>chart.render</code>{" "}
                                with a Vega-Lite spec (matplotlib intentionally
                                not installed).
                            </li>
                        </ul>
                    </section>
                </>
            )}
        </div>
    );
}

/// Single-row chip for the kernel-gateway sidecar's status. Mirrors
/// the SidecarsPage chip mapping so the same colors mean the same
/// things across both pages.
function SidecarChip({ sidecar }: { sidecar: SidecarStatusView }) {
    const variant =
        sidecar.status === "healthy"
            ? "success"
            : sidecar.status === "crashlooping" || sidecar.status === "notfound"
              ? "danger"
              : "warning";
    return (
        <div className="d-flex align-items-center gap-3 small">
            <span
                className={`badge text-bg-${variant}`}
                data-testid="python-sandbox-sidecar-badge"
            >
                {sidecar.status}
            </span>
            {sidecar.rpc_url && (
                <code className="text-body-secondary">{sidecar.rpc_url}</code>
            )}
            {sidecar.restart_attempts > 0 && (
                <span className="text-body-secondary">
                    restart attempts: {sidecar.restart_attempts}
                </span>
            )}
        </div>
    );
}
