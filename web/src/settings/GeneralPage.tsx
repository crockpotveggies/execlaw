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
    updateGeneralSettings,
    type GeneralSettings,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

export function GeneralPage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [settings, setSettings] = useState<GeneralSettings | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [bindAddress, setBindAddress] = useState("");
    const [startOnBoot, setStartOnBoot] = useState(true);
    /// Tracks whether the operator has changed bind_address since
    /// the last load — drives the "service restart required" hint.
    const [bindDirty, setBindDirty] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await getGeneralSettings(getToken);
            setSettings(r);
            setBindAddress(r.bind_address);
            setStartOnBoot(r.start_on_boot);
            setBindDirty(false);
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
            const body: { start_on_boot?: boolean; bind_address?: string } = {};
            if (settings && settings.start_on_boot !== startOnBoot) {
                body.start_on_boot = startOnBoot;
            }
            if (settings && settings.bind_address !== bindAddress.trim()) {
                body.bind_address = bindAddress.trim();
            }
            if (Object.keys(body).length === 0) {
                setBusy(false);
                return;
            }
            const r = await updateGeneralSettings(body, getToken);
            setSettings(r);
            setBindAddress(r.bind_address);
            setStartOnBoot(r.start_on_boot);
            setBindDirty(false);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [settings, startOnBoot, bindAddress, getToken]);

    const dirty =
        !!settings &&
        (settings.start_on_boot !== startOnBoot ||
            settings.bind_address !== bindAddress.trim());

    return (
        <div data-testid="settings-general">
            <h3 className="h6 mb-3">General</h3>
            <p className="execlaw-muted small mb-3">
                Operator settings for the host service. The control plane
                runs as a systemd / launchd / Windows service — see{" "}
                <code>execlaw service status</code> from a terminal for
                live state and log paths.
            </p>

            {!canMutate && (
                <div className="execlaw-muted small mb-3">
                    Read-only view. Only Controllers can change general
                    settings.
                </div>
            )}

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {settings === null ? (
                <div className="execlaw-muted small">Loading…</div>
            ) : (
                <div className="execlaw-card" data-testid="general-form">
                    <Form.Group className="mb-3">
                        <Form.Check
                            type="switch"
                            id="general-start-on-boot"
                            label="Start at boot"
                            checked={startOnBoot}
                            disabled={!canMutate || busy}
                            onChange={(e) => setStartOnBoot(e.target.checked)}
                            data-testid="general-start-on-boot"
                        />
                        <Form.Text className="execlaw-muted">
                            When on, the host service launches automatically
                            at OS boot. The toggle is honoured by the next{" "}
                            <code>execlaw service install</code> run; the
                            service-manager registration on disk doesn't
                            change until then.
                        </Form.Text>
                    </Form.Group>

                    <Form.Group className="mb-3">
                        <Form.Label className="execlaw-muted small mb-1">
                            Bind address (host:port)
                        </Form.Label>
                        <Form.Control
                            value={bindAddress}
                            onChange={(e) => {
                                setBindAddress(e.target.value);
                                setBindDirty(true);
                            }}
                            placeholder="127.0.0.1:3030"
                            disabled={!canMutate || busy}
                            data-testid="general-bind-address"
                        />
                        <Form.Text className="execlaw-muted">
                            The address the control plane listens on. Use{" "}
                            <code>127.0.0.1:3030</code> for loopback only,{" "}
                            <code>0.0.0.0:3030</code> to bind every
                            interface (put a reverse proxy in front for TLS),
                            or an IPv6 literal like{" "}
                            <code>[::1]:3030</code>.
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
                                Bind address takes effect on the next{" "}
                                <code>execlaw service restart</code>.
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
                                Save
                            </Button>
                            {dirty && (
                                <Button
                                    variant="outline-secondary"
                                    disabled={busy}
                                    onClick={() => void refresh()}
                                    data-testid="general-revert"
                                >
                                    Revert
                                </Button>
                            )}
                        </div>
                    )}
                </div>
            )}
        </div>
    );
}
