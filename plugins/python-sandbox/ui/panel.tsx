// Python-sandbox plugin self-contained config panel.
//
// Loaded at runtime by the host's `DynamicPluginPanel` when the
// operator visits `/settings/plugins/python-sandbox`. The host
// fetches this file (post-build → `ui/panel.js`) via
// `GET /api/admin/plugins/python-sandbox/ui/panel.js`, wraps the
// response in a Blob URL, and `import()`s it. The default-exported
// React component is then mounted in place; React + helpers +
// shared components come from the `bridge` prop so we never ship
// a duplicate React copy.
//
// Build from this source:
//   npm --prefix web run build-plugin -- python-sandbox
// Watch mode:
//   npm --prefix web run build-plugin -- python-sandbox --watch
//
// What this panel shows:
//
//   * **Sidecar status** — `SidecarStatusBlock` rendering the
//     `kernel-gateway` container's health from
//     `GET /api/admin/sidecars`. Lets the operator see "Healthy"
//     vs "CrashLooping" + the RPC port at a glance.
//
//   * **Settings** — two operator-editable knobs persisted via the
//     host's generic plugin-settings store:
//       - `kernel_idle_timeout_seconds` (default 900) — how long
//         an inactive kernel stays alive before eviction. Tradeoff:
//         lower means less memory consumed by idle kernels at the
//         cost of a cold start on the next turn; higher means
//         faster repeats at the cost of memory.
//       - `max_output_bytes` (default 52428800 = 50 MB) — hard cap
//         on a single `python.execute` output. Exceeding this
//         aborts the kernel + returns `status: output_too_large`.
//
//     Changes take effect on the NEXT server restart (settings are
//     read in `wire_python_sandbox` at boot). The form makes this
//     explicit with a sentinel hint.
//
//   * **About** — version, what the plugin does, the three places
//     the agent interacts with the filesystem (`/work/uploads/`,
//     `/work/outputs/`, kernel state). Useful at-a-glance for an
//     operator wondering "what does this plugin let the agent do?"

import type {
    PluginPanelComponent,
    PluginPanelProps,
} from "@execlaw/plugin-ui";

const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

// --- API types ------------------------------------------------------

/// Shape of one entry returned by `GET /api/admin/sidecars`. We
/// only consume the `name`, `status`, and `rpc_url` fields; the
/// rest are tolerated as unknown so the type doesn't break when
/// the host evolves the schema.
interface SidecarView {
    name: string;
    plugin_id: string;
    status: string;
    rpc_url: string | null;
}

interface SidecarListResponse {
    sidecars: SidecarView[];
}

/// Shape of `GET /api/admin/plugins/{id}/settings/{key}`. Returns
/// 200 with `{value: "<utf-8 string>"}` when set, 404 when not.
/// The plugin decides interpretation — we treat values as decimal
/// integers since both our knobs are byte/second counts.
interface SettingValueResponse {
    value: string;
}

// --- Constants ------------------------------------------------------

const SIDECAR_NAME = "kernel-gateway";

const DEFAULT_IDLE_TIMEOUT_SECONDS = 900;
const DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024 * 1024;

/// Inclusive bounds for the operator's input. Idle timeout below
/// 60s makes the kernel pool thrash; above 24h is a config error
/// (process restart should bound it). Output cap below 1 MB is
/// useless for any real `python.execute` workload; above 500 MB
/// risks the host running out of memory before the cap triggers.
const IDLE_TIMEOUT_MIN = 60;
const IDLE_TIMEOUT_MAX = 24 * 60 * 60;
const MAX_OUTPUT_MIN = 1024 * 1024;
const MAX_OUTPUT_MAX = 500 * 1024 * 1024;

// --- Top-level Panel ------------------------------------------------

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { bridge, identity } = props;
    const { ErrorBanner, SidecarStatusBlock, Button } = bridge.components;

    const [sidecar, setSidecar] = useState<SidecarView | null>(null);
    const [sidecarError, setSidecarError] = useState<string | null>(null);
    // 2026-05-18 — separate "has a fetch ever completed" from "is
    // there a row in the response". A plain `sidecar === null` check
    // can't distinguish "haven't fetched yet" from "fetched, no
    // matching row" — the panel was stuck on "Loading…" forever in
    // the latter case (which fires when the sidecar plugin is
    // installed but isn't registered as a supervised container yet,
    // or when an id mismatch made `.find()` miss).
    const [sidecarLoaded, setSidecarLoaded] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const [idleTimeout, setIdleTimeout] = useState<number>(
        DEFAULT_IDLE_TIMEOUT_SECONDS,
    );
    const [maxOutput, setMaxOutput] = useState<number>(DEFAULT_MAX_OUTPUT_BYTES);
    const [settingsLoaded, setSettingsLoaded] = useState(false);
    const [saving, setSaving] = useState(false);
    const [saved, setSaved] = useState(false);

    // --- Sidecar poll. 3s matches the cadence Signal uses; the
    // operator visiting this page is usually monitoring a recently-
    // restarted sidecar, so a faster cadence is wasted effort.
    const refreshSidecar = useCallback(async () => {
        try {
            const resp = await bridge.fetchJson<SidecarListResponse>(
                "GET",
                "/api/admin/sidecars",
            );
            const found = resp.sidecars.find(
                (s) => s.plugin_id === identity.id && s.name === SIDECAR_NAME,
            );
            setSidecar(found ?? null);
            setSidecarError(null);
        } catch (e) {
            setSidecarError(e instanceof Error ? e.message : String(e));
        } finally {
            // Flip the "loaded" flag regardless of success / miss /
            // error so the render path can exit "Loading…" and
            // show either the chip or an explicit empty state.
            setSidecarLoaded(true);
        }
    }, [bridge, identity.id]);

    useEffect(() => {
        void refreshSidecar();
        const id = window.setInterval(() => {
            void refreshSidecar();
        }, 3_000);
        return () => window.clearInterval(id);
    }, [refreshSidecar]);

    // --- Settings load. One-shot; the per-key GET 404s when not
    // set yet, in which case we keep the defaults. Both keys
    // fetched in parallel to keep the first paint snappy.
    useEffect(() => {
        let cancelled = false;
        const loadOne = async (key: string): Promise<string | null> => {
            try {
                const r = await bridge.fetchJson<SettingValueResponse>(
                    "GET",
                    `/api/admin/plugins/${identity.id}/settings/${key}`,
                );
                return r.value;
            } catch (e) {
                // 404 = not set yet; treat as "use default".
                // Other errors bubble up via the banner.
                if (
                    e instanceof Error &&
                    (e.message.includes("404") || e.message.includes("not_found"))
                ) {
                    return null;
                }
                throw e;
            }
        };
        const run = async () => {
            try {
                const [idle, output] = await Promise.all([
                    loadOne("kernel_idle_timeout_seconds"),
                    loadOne("max_output_bytes"),
                ]);
                if (cancelled) return;
                if (idle !== null) {
                    const n = Number.parseInt(idle, 10);
                    if (Number.isFinite(n)) setIdleTimeout(n);
                }
                if (output !== null) {
                    const n = Number.parseInt(output, 10);
                    if (Number.isFinite(n)) setMaxOutput(n);
                }
                setSettingsLoaded(true);
            } catch (e) {
                if (cancelled) return;
                setError(e instanceof Error ? e.message : String(e));
                setSettingsLoaded(true); // still allow editing on defaults
            }
        };
        void run();
        return () => {
            cancelled = true;
        };
    }, [bridge, identity.id]);

    const onSave = useCallback(
        async (e: React.FormEvent) => {
            e.preventDefault();
            // Validate before any network round-trip — the host's
            // store will accept any UTF-8 string, so client-side
            // is the only place we enforce the sensible-range
            // constraints. Errors surface inline.
            if (
                idleTimeout < IDLE_TIMEOUT_MIN ||
                idleTimeout > IDLE_TIMEOUT_MAX ||
                !Number.isInteger(idleTimeout)
            ) {
                setError(
                    `Idle timeout must be an integer between ${IDLE_TIMEOUT_MIN} ` +
                        `and ${IDLE_TIMEOUT_MAX} seconds.`,
                );
                return;
            }
            if (
                maxOutput < MAX_OUTPUT_MIN ||
                maxOutput > MAX_OUTPUT_MAX ||
                !Number.isInteger(maxOutput)
            ) {
                setError(
                    `Max output must be an integer between ${MAX_OUTPUT_MIN} ` +
                        `and ${MAX_OUTPUT_MAX} bytes.`,
                );
                return;
            }
            setSaving(true);
            setError(null);
            setSaved(false);
            try {
                await Promise.all([
                    bridge.fetchJson<unknown>(
                        "PUT",
                        `/api/admin/plugins/${identity.id}/settings/kernel_idle_timeout_seconds`,
                        { value: String(idleTimeout) },
                    ),
                    bridge.fetchJson<unknown>(
                        "PUT",
                        `/api/admin/plugins/${identity.id}/settings/max_output_bytes`,
                        { value: String(maxOutput) },
                    ),
                ]);
                setSaved(true);
                // Clear the "Saved" hint after a few seconds so the
                // form doesn't look stale.
                window.setTimeout(() => setSaved(false), 4_000);
            } catch (err) {
                setError(err instanceof Error ? err.message : String(err));
            } finally {
                setSaving(false);
            }
        },
        [bridge, identity.id, idleTimeout, maxOutput],
    );

    return (
        <div data-testid="python-sandbox-config-page">
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-3"
            />

            {/* --- Sidecar status ------------------------------- */}
            <h3 className="h6 mb-2">Kernel gateway</h3>
            {!sidecarLoaded ? (
                <div className="execlaw-muted small mb-3">Loading sidecar status…</div>
            ) : (
                <SidecarStatusBlock
                    sidecarLabel={SIDECAR_NAME}
                    status={sidecar?.status ?? "not_registered"}
                    rpcUrl={sidecar?.rpc_url ?? null}
                    fetchError={sidecarError}
                    testidPrefix="python-sandbox"
                />
            )}

            {/* --- Settings ------------------------------------- */}
            <h3 className="h6 mt-4 mb-2">Settings</h3>
            <p className="text-body-secondary small mb-3">
                Changes take effect on the next server restart — the host
                reads these at boot when wiring the python-sandbox service.
            </p>
            <form onSubmit={onSave} data-testid="python-sandbox-settings-form">
                <div className="mb-3">
                    <label
                        htmlFor="ps-idle-timeout"
                        className="form-label small fw-semibold"
                    >
                        Kernel idle timeout (seconds)
                    </label>
                    <input
                        id="ps-idle-timeout"
                        type="number"
                        className="form-control form-control-sm"
                        value={idleTimeout}
                        min={IDLE_TIMEOUT_MIN}
                        max={IDLE_TIMEOUT_MAX}
                        step={1}
                        disabled={!settingsLoaded || saving}
                        onChange={(ev) => {
                            const n = Number.parseInt(ev.target.value, 10);
                            if (Number.isFinite(n)) setIdleTimeout(n);
                        }}
                        data-testid="python-sandbox-idle-timeout-input"
                    />
                    <div className="form-text small">
                        How long an inactive kernel stays alive before the pool
                        evicts it. Default: {DEFAULT_IDLE_TIMEOUT_SECONDS}s (15 min).
                        Lower = less idle memory, more cold starts.
                    </div>
                </div>
                <div className="mb-3">
                    <label
                        htmlFor="ps-max-output"
                        className="form-label small fw-semibold"
                    >
                        Max output bytes per execute
                    </label>
                    <input
                        id="ps-max-output"
                        type="number"
                        className="form-control form-control-sm"
                        value={maxOutput}
                        min={MAX_OUTPUT_MIN}
                        max={MAX_OUTPUT_MAX}
                        step={1024 * 1024}
                        disabled={!settingsLoaded || saving}
                        onChange={(ev) => {
                            const n = Number.parseInt(ev.target.value, 10);
                            if (Number.isFinite(n)) setMaxOutput(n);
                        }}
                        data-testid="python-sandbox-max-output-input"
                    />
                    <div className="form-text small">
                        Hard cap on a single python.execute output. Default:{" "}
                        {(DEFAULT_MAX_OUTPUT_BYTES / 1024 / 1024).toFixed(0)} MB.
                        Exceeding this aborts the kernel and returns{" "}
                        <code>status: output_too_large</code>.
                    </div>
                </div>
                <div className="d-flex align-items-center gap-2">
                    <Button
                        type="submit"
                        variant="primary"
                        disabled={!settingsLoaded || saving}
                        data-testid="python-sandbox-save"
                    >
                        {saving ? "Saving…" : "Save settings"}
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
            </form>

            {/* --- About --------------------------------------- */}
            <h3 className="h6 mt-4 mb-2">About</h3>
            <p className="small text-body-secondary mb-2">
                Persistent per-conversation Python execution sandbox. Pre-
                installed: pandas, polars, duckdb, pyarrow, numpy, openpyxl,
                ipython, httpx. Cannot pip install additional packages.
            </p>
            <ul className="small text-body-secondary mb-0">
                <li>
                    <code>/work/uploads/</code> — files the operator attached to
                    this conversation; read-only inside the kernel.
                </li>
                <li>
                    <code>/work/outputs/</code> — agent writes files here, host
                    auto-publishes them as conversation artifacts.
                </li>
                <li>
                    Kernel state (variables, dataframes, imports) survives
                    across turns until the idle-timeout above expires.
                </li>
                <li>
                    Charts: agent computes data with pandas/polars/duckdb, then
                    calls <code>chart.render</code> with a Vega-Lite spec
                    (matplotlib intentionally not installed).
                </li>
            </ul>
        </div>
    );
};

export default Panel;
