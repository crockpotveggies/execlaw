// Settings → General (Phase 14 — bare-metal pivot).
//
// Operator-editable knobs that don't fit any of the per-feature
// pages. v1 ships:
//
//   * start_on_boot — wired into the host service registration.
//                     Toggle re-runs `execlaw service install`
//                     with the new autostart flag on next launch.
//   * bind_address  — host:port the service binds. Edits don't
//                     restart the running process; SPA shows a
//                     "Restart required" hint and the operator
//                     runs `execlaw service restart` from a
//                     terminal.
//
// The page is intentionally small — most settings have their own
// page already (Backends, Personality, Trust Policy). This is the
// catch-all for OS-service-shaped knobs.

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    getGeneralSettings,
    postFactoryReset,
    updateGeneralSettings,
    HISTORY_RETENTION_OPTIONS,
    type GeneralSettings,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";
import {
    LANGUAGE_OPTIONS,
    setLanguage,
    useCurrentLanguage,
    useT,
} from "../i18n";

/// Literal the operator must type into the danger-zone input. Kept
/// in sync with `factory_reset::CONFIRM_TOKEN` server-side; if these
/// drift the server returns 400 and the SPA surfaces the error.
const FACTORY_RESET_CONFIRM = "RESET";

export function GeneralPage() {
    const auth = useAuth();
    const t = useT();
    const getToken = auth.getAccessToken;
    const [settings, setSettings] = useState<GeneralSettings | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [bindAddress, setBindAddress] = useState("");
    const [startOnBoot, setStartOnBoot] = useState(true);
    const [retentionDays, setRetentionDays] = useState(30);
    /// Tracks whether the operator has changed bind_address since
    /// the last load — drives the "service restart required" hint.
    const [bindDirty, setBindDirty] = useState(false);
    /// Same shape for `start_on_boot` — the autostart flag on the
    /// installed service unit only updates on the next
    /// `execlaw service install` run, so the toggle persists to the
    /// DB but doesn't immediately re-register.
    const [bootDirty, setBootDirty] = useState(false);
    /// Whether the operator changed retention since load. Used so
    /// the "narrowing window" warning only renders on a real edit.
    const [retentionDirty, setRetentionDirty] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await getGeneralSettings(getToken);
            setSettings(r);
            setBindAddress(r.bind_address);
            setStartOnBoot(r.start_on_boot);
            setRetentionDays(r.history_retention_days);
            setBindDirty(false);
            setBootDirty(false);
            setRetentionDirty(false);
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

    const onSave = useCallback(async () => {
        setBusy(true);
        setError(null);
        try {
            const body: {
                start_on_boot?: boolean;
                bind_address?: string;
                history_retention_days?: number;
            } = {};
            if (settings && settings.start_on_boot !== startOnBoot) {
                body.start_on_boot = startOnBoot;
            }
            if (settings && settings.bind_address !== bindAddress.trim()) {
                body.bind_address = bindAddress.trim();
            }
            if (settings && settings.history_retention_days !== retentionDays) {
                body.history_retention_days = retentionDays;
            }
            if (Object.keys(body).length === 0) {
                setBusy(false);
                return;
            }
            const r = await updateGeneralSettings(body, getToken);
            setSettings(r);
            setBindAddress(r.bind_address);
            setStartOnBoot(r.start_on_boot);
            setRetentionDays(r.history_retention_days);
            setBindDirty(false);
            setBootDirty(false);
            setRetentionDirty(false);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [settings, startOnBoot, bindAddress, retentionDays, getToken]);

    const dirty =
        !!settings &&
        (settings.start_on_boot !== startOnBoot ||
            settings.bind_address !== bindAddress.trim() ||
            settings.history_retention_days !== retentionDays);

    /// Whether the operator's pending change shrinks the retention
    /// window. Drives a confirm-style warning since shorter retention
    /// triggers a one-time bulk delete on the next sweep tick.
    const retentionNarrowing =
        !!settings &&
        retentionDirty &&
        // 0 (Infinite) is the widest possible — never narrowing.
        retentionDays !== 0 &&
        (settings.history_retention_days === 0 ||
            retentionDays < settings.history_retention_days);

    return (
        <div data-testid="settings-general">
            <h3 className="h6 mb-3">{t("general.title", "General")}</h3>
            <p className="execlaw-muted small mb-3">
                {t(
                    "general.introPre",
                    "Operator settings for the host service. The control plane runs as a systemd / launchd / Windows service — see ",
                )}
                <code>execlaw service status</code>
                {t(
                    "general.introPost",
                    " from a terminal for live state and log paths.",
                )}
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    {t(
                        "general.readOnly",
                        "Read-only view. Only Controllers can change general settings.",
                    )}
                </div>
            )}

            <LanguagePicker />

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            {settings === null ? (
                <div className="execlaw-muted small">
                    {t("general.loading", "Loading…")}
                </div>
            ) : (
                <div className="execlaw-card" data-testid="general-form">
                    <Form.Group className="mb-3">
                        <Form.Check
                            type="switch"
                            id="general-start-on-boot"
                            label={t("general.startAtBoot", "Start at boot")}
                            checked={startOnBoot}
                            disabled={!canMutate || busy}
                            onChange={(e) => {
                                setStartOnBoot(e.target.checked);
                                setBootDirty(true);
                            }}
                            data-testid="general-start-on-boot"
                        />
                        <Form.Text className="execlaw-muted">
                            {t(
                                "general.startAtBootHelpPre",
                                "When on, the host service launches automatically at OS boot. The toggle is honoured by the next ",
                            )}
                            <code>execlaw service install</code>
                            {t(
                                "general.startAtBootHelpPost",
                                " run; the service-manager registration on disk doesn’t change until then.",
                            )}
                        </Form.Text>
                        {bootDirty && (
                            <div
                                className="execlaw-muted small mt-2"
                                data-testid="general-boot-reinstall-hint"
                            >
                                <i
                                    className="bi bi-info-circle me-1"
                                    aria-hidden
                                />
                                {t("general.bootReinstallPre", "Re-run ")}
                                <code>execlaw service install</code>
                                {t(
                                    "general.bootReinstallPost",
                                    " from a terminal to apply the new autostart flag (or an elevated PowerShell on Windows).",
                                )}
                            </div>
                        )}
                    </Form.Group>

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            {t("general.bindAddress", "Bind address (host:port)")}
                        </Form.Label>
                        <Form.Control
                            value={bindAddress}
                            onChange={(e) => {
                                setBindAddress(e.target.value);
                                setBindDirty(true);
                            }}
                            placeholder="127.0.0.1:3031"
                            disabled={!canMutate || busy}
                            data-testid="general-bind-address"
                        />
                        <Form.Text className="execlaw-muted">
                            {t(
                                "general.bindAddressHelp",
                                "The address the control plane listens on. Use 127.0.0.1:3031 for loopback only, 0.0.0.0:3031 to bind every interface (put a reverse proxy in front for TLS), or an IPv6 literal like [::1]:3031.",
                            )}
                        </Form.Text>
                        {bindDirty && settings.bind_address_requires_restart && (
                            <div
                                className="execlaw-muted small mt-2"
                                data-testid="general-bind-restart-hint"
                            >
                                <i
                                    className="bi bi-info-circle me-1"
                                    aria-hidden
                                />
                                {t(
                                    "general.bindRestartHintPre",
                                    "Bind address takes effect on the next ",
                                )}
                                <code>execlaw service restart</code>
                                {t("general.bindRestartHintPost", ".")}
                            </div>
                        )}
                    </Form.Group>

                    <Form.Group className="mb-3">
                        <Form.Label
                            className="execlaw-muted small mb-1"
                            htmlFor="general-history-retention"
                        >
                            {t("general.historyRetention", "History retention")}
                        </Form.Label>
                        <Form.Select
                            id="general-history-retention"
                            value={retentionDays}
                            disabled={!canMutate || busy}
                            onChange={(e) => {
                                setRetentionDays(Number(e.target.value));
                                setRetentionDirty(true);
                            }}
                            data-testid="general-history-retention"
                        >
                            {HISTORY_RETENTION_OPTIONS.map((opt) => (
                                <option key={opt.value} value={opt.value}>
                                    {opt.label}
                                </option>
                            ))}
                        </Form.Select>
                        <Form.Text className="execlaw-muted">
                            {t(
                                "general.historyRetentionHelp",
                                "How long conversation history, scheduled-task runs, structured logs, and (forthcoming) research jobs stick around before automatic deletion. Memory entries and the audit log are not affected.",
                            )}
                        </Form.Text>
                        {retentionNarrowing && (
                            <div
                                className="execlaw-muted small mt-2"
                                data-testid="general-retention-narrowing-hint"
                            >
                                <i
                                    className="bi bi-exclamation-triangle me-1"
                                    aria-hidden
                                />
                                {t(
                                    "general.retentionNarrowing",
                                    "This is a shorter window. Saving will trigger a one-time bulk deletion of older history on the next sweep tick (within ~30 minutes). This cannot be undone.",
                                )}
                            </div>
                        )}
                    </Form.Group>

                    {canMutate && (
                        <div className="d-flex gap-2">
                            <Button
                                variant="primary"
                                disabled={busy || !dirty}
                                onClick={() => void onSave()}
                                data-testid="general-save"
                            >
                                {t("general.save", "Save")}
                            </Button>
                            {dirty && (
                                <Button
                                    variant="outline-secondary"
                                    disabled={busy}
                                    onClick={() => void refresh()}
                                    data-testid="general-revert"
                                >
                                    {t("general.revert", "Revert")}
                                </Button>
                            )}
                        </div>
                    )}
                </div>
            )}

            {canMutate && <DangerZone />}
        </div>
    );
}

/// Per-client language picker. Persists to localStorage via the i18n
/// module (key: `execlaw.preferred-language`), so it's a UI
/// preference rather than server state — every role can change it,
/// and changes apply immediately without a Save button. The
/// pre-auth surfaces (Login + SetupWizard) read the same key via
/// the floating LanguageSwitcher chip.
function LanguagePicker() {
    const t = useT();
    const current = useCurrentLanguage();
    return (
        <div className="execlaw-card mb-3" data-testid="general-language-card">
            <Form.Group>
                <Form.Label
                    className="execlaw-muted small mb-1"
                    htmlFor="general-language"
                >
                    {t("general.language", "Language")}
                </Form.Label>
                <Form.Select
                    id="general-language"
                    value={current}
                    onChange={(e) => {
                        void setLanguage(e.target.value);
                    }}
                    data-testid="general-language"
                >
                    {LANGUAGE_OPTIONS.map((o) => (
                        <option key={o.value} value={o.value}>
                            {o.label}
                        </option>
                    ))}
                </Form.Select>
                <Form.Text className="execlaw-muted">
                    {t(
                        "general.languageHelp",
                        "Stored per browser. Detection falls back to your OS language on first run; changes here override it.",
                    )}
                </Form.Text>
            </Form.Group>
        </div>
    );
}

/// Bottom-of-page card that wipes every persistent table and signs the
/// operator out. Two-step confirm: (1) operator types the literal
/// `RESET` into the input — only then is the button enabled —, then
/// (2) a native `window.confirm()` blocks the destructive call. After
/// a 200, we sign-out and route to /login; the AppBoot guard there
/// detects the missing controller user and bounces to /setup.
function DangerZone() {
    const auth = useAuth();
    const t = useT();
    const getToken = auth.getAccessToken;
    const [confirmText, setConfirmText] = useState("");
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const armed = confirmText === FACTORY_RESET_CONFIRM;

    const onReset = useCallback(async () => {
        if (!armed) return;
        const ok = window.confirm(
            t(
                "general.dangerConfirm",
                "Wipe ALL data and factory-reset the service?\n\nThis deletes every conversation, plugin, contact, routine, oauth token, and operator account. The service stays running but you'll be signed out and the next visit will start the setup wizard.\n\nThis cannot be undone.",
            ),
        );
        if (!ok) return;
        setBusy(true);
        setError(null);
        try {
            await postFactoryReset(FACTORY_RESET_CONFIRM, getToken);
            // Clear local auth state then hard-reload to "/" so
            // every cached selector / store / WS resets. The AppBoot
            // guard sees no controller user and routes to /setup.
            // Hard reload (instead of useNavigate) is deliberate:
            // after a destructive wipe we want a clean React tree
            // and a fresh WebSocket handshake.
            await auth.signOut();
            window.location.assign("/login");
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setBusy(false);
        }
    }, [armed, auth, getToken, t]);

    return (
        <div
            className="execlaw-card mt-4 border border-danger-subtle"
            data-testid="general-danger-zone"
        >
            <div className="execlaw-card__title text-danger">
                <i className="bi bi-exclamation-triangle me-2" aria-hidden />
                {t("general.dangerZone", "Danger zone")}
            </div>
            <div className="execlaw-muted small mb-2">
                {t(
                    "general.dangerZoneBody",
                    "Erases every conversation, plugin, contact, routine, stored token, and operator account, then signs you out. The service keeps running but the next visit starts at the setup wizard. Filesystem artifacts (research workspaces, attachments) are not removed; restart the host service for a truly clean slate.",
                )}
            </div>
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mb-2"
            />
            <Form.Group className="mb-2">
                <Form.Label
                    className="execlaw-muted small mb-1"
                    htmlFor="general-danger-confirm"
                >
                    {t("general.dangerTypePre", "Type ")}
                    <code data-testid="general-danger-token">
                        {FACTORY_RESET_CONFIRM}
                    </code>
                    {t("general.dangerTypePost", " to enable the button.")}
                </Form.Label>
                <Form.Control
                    id="general-danger-confirm"
                    size="sm"
                    value={confirmText}
                    onChange={(e) => setConfirmText(e.target.value)}
                    placeholder={FACTORY_RESET_CONFIRM}
                    disabled={busy}
                    autoComplete="off"
                    spellCheck={false}
                    data-testid="general-danger-confirm-input"
                />
            </Form.Group>
            <Button
                variant="outline-danger"
                size="sm"
                disabled={!armed || busy}
                onClick={() => void onReset()}
                data-testid="general-danger-reset"
            >
                {busy ? (
                    <>
                        <i
                            className="bi bi-hourglass-split me-1"
                            aria-hidden
                        />
                        {t("general.wiping", "Wiping…")}
                    </>
                ) : (
                    <>
                        <i className="bi bi-trash3 me-1" aria-hidden />
                        {t(
                            "general.dangerReset",
                            "Erase all data and factory reset",
                        )}
                    </>
                )}
            </Button>
        </div>
    );
}
