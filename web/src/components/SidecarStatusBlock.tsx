// Shared "sidecar status" card for plugin config pages.
//
// Renders the operator-facing view of where the supervised container
// is in its lifecycle: pulling the Docker image, container started but
// RPC not yet healthy, healthy, crash-looping, stopped. Replaces the
// near-identical SidecarStatusBlock that previously lived inline in
// SignalConfigPage and WhatsAppConfigPage.
//
// The headline UX improvement: when the sidecar is in `pulling` or
// `starting` state, the operator sees an explicit "Booting up…"
// header with an inline spinner + a stage-appropriate explainer of
// what's normal vs concerning. Without this, an operator who lands
// on the page during the 30-second-ish first-boot window sees raw
// status text ("starting") and can't tell whether to wait or
// restart.
//
// The block is stateless — the parent page polls
// `/api/admin/plugins/<id>/status` on its own cadence and re-renders
// us with fresh props. We don't trigger any fetches.

import Spinner from "react-bootstrap/Spinner";
import type { JSX, ReactNode } from "react";

export interface SidecarStatusProps {
    /// Display name for the sidecar binary, e.g. "signal-cli" or
    /// "wuzapi". Renders as the card title.
    sidecarLabel: string;
    /// Raw status string returned by the supervisor — one of
    /// `pulling | starting | healthy | crash_looping | stopped |
    /// not_found | unwired | awaiting_pairing | <other>`. Unknown
    /// values fall through to a neutral "Unknown" badge.
    status: string;
    /// Loopback URL the supervisor published. `null` while the
    /// sidecar is pre-healthy.
    rpcUrl: string | null;
    /// Plugin-side fetch error (e.g. plugin's per-sidecar bootstrap
    /// call failed). Surfaces below the chip when populated.
    fetchError?: string | null;
    /// Optional plugin-specific hint to render INSTEAD of the default
    /// explainer for the current status. WhatsApp uses this for the
    /// `awaiting_pairing` state where the sidecar is up but the
    /// per-user wuzapi auth hasn't been provisioned.
    followupHint?: ReactNode;
    /// Test-id prefix (`signal` or `whatsapp`). Block uses
    /// `${prefix}-sidecar-block`, chip uses `${prefix}-sidecar-status`.
    testidPrefix: string;
}

interface StatusPresentation {
    label: string;
    badgeClass: string;
    showSpinner: boolean;
    explainer: ReactNode | null;
    /// True when this status represents a normal in-progress boot;
    /// false when it's a terminal / stuck state. Drives the
    /// "Booting up…" header treatment.
    isBooting: boolean;
}

function presentationFor(status: string): StatusPresentation {
    // Accept both `crash_looping` (the supervisor's wire format —
    // see `crates/server/src/sidecars_admin.rs::view_from_status`)
    // and the legacy `crashlooping` spelling that an older snapshot
    // of this component coalesced through. Same display either way.
    switch (status) {
        case "pulling":
            return {
                label: "Pulling image",
                badgeClass: "bg-info",
                showSpinner: true,
                isBooting: true,
                explainer: (
                    <>
                        Downloading the Docker image. First run can take
                        <strong> 10–60 seconds</strong> depending on
                        connection speed. After this completes the
                        container starts; pairing becomes available
                        when status reaches <strong>healthy</strong>.
                    </>
                ),
            };
        case "starting":
            return {
                label: "Starting",
                badgeClass: "bg-info",
                showSpinner: true,
                isBooting: true,
                explainer: (
                    <>
                        Container is up; waiting for the service inside
                        it to respond to the supervisor&apos;s health
                        probe. Usually a few seconds. The page polls
                        every few seconds and will update on its own.
                    </>
                ),
            };
        case "healthy":
            return {
                label: "Healthy",
                badgeClass: "bg-success",
                showSpinner: false,
                isBooting: false,
                explainer: null,
            };
        case "crash_looping":
        case "crashlooping":
            return {
                label: "Crash looping",
                badgeClass: "bg-danger",
                showSpinner: false,
                isBooting: false,
                explainer: (
                    <>
                        Sidecar failed to come up after multiple restart
                        attempts and is parked. Check{" "}
                        <strong>Settings → Sidecars</strong> for the
                        recent error log; common causes are a port
                        already bound on the host or a stale Docker
                        volume left from a previous install.
                    </>
                ),
            };
        case "stopped":
            return {
                label: "Stopped",
                badgeClass: "bg-secondary",
                showSpinner: false,
                isBooting: false,
                explainer: (
                    <>
                        Sidecar isn&apos;t running. Restart from{" "}
                        <strong>Settings → Sidecars</strong> or by
                        disabling and re-enabling the plugin.
                    </>
                ),
            };
        case "not_found":
        case "unwired":
            return {
                label: "Not registered",
                badgeClass: "bg-secondary",
                showSpinner: false,
                isBooting: false,
                explainer: (
                    <>
                        Supervisor doesn&apos;t know about this sidecar
                        yet. If you just enabled the plugin, refresh
                        in a second; otherwise try disabling and
                        re-enabling from <strong>Settings →
                        Plugins</strong>.
                    </>
                ),
            };
        case "awaiting_pairing":
            // Plugin-defined "sidecar is up but the per-tenant
            // upstream account hasn't been provisioned yet" state
            // (WhatsApp uses this between sidecar-healthy and
            // wuzapi-user-created). The SIDECAR is fully booted at
            // this point, so DO NOT show the "Sidecar is booting
            // up…" header — the chip + caller's `followupHint`
            // explain the next-step state. The chip stays bg-info
            // because the operator's expected action is "wait" not
            // "fix".
            return {
                label: "Awaiting pairing",
                badgeClass: "bg-info",
                showSpinner: true,
                isBooting: false,
                explainer: null,
            };
        default:
            return {
                label: status || "Unknown",
                badgeClass: "bg-secondary",
                showSpinner: false,
                isBooting: false,
                explainer: null,
            };
    }
}

export function SidecarStatusBlock({
    sidecarLabel,
    status,
    rpcUrl,
    fetchError,
    followupHint,
    testidPrefix,
}: SidecarStatusProps): JSX.Element {
    const p = presentationFor(status);
    return (
        <div
            className="execlaw-card mb-3"
            data-testid={`${testidPrefix}-sidecar-block`}
        >
            <div className="execlaw-card__title mb-2">{sidecarLabel} sidecar</div>

            {/*
              Booting-state header. Renders ABOVE the chip so the
              operator's eye lands on "Booting up…" first instead of
              having to interpret a raw status word. Only shown for
              `pulling` / `starting` / `awaiting_pairing` — terminal
              states (healthy, crash_looping, stopped) skip the header.
            */}
            {p.isBooting && (
                <div
                    className="d-flex align-items-center gap-2 mb-2 fw-semibold"
                    data-testid={`${testidPrefix}-sidecar-booting-header`}
                >
                    <Spinner
                        size="sm"
                        animation="border"
                        role="status"
                        aria-label="Sidecar booting up"
                    />
                    <span>Sidecar is booting up…</span>
                </div>
            )}

            <div className="d-flex align-items-center gap-2 small mb-2 flex-wrap">
                <span
                    className={`badge ${p.badgeClass}`}
                    data-testid={`${testidPrefix}-sidecar-status`}
                    /* The raw status is exposed via data-attr too so
                       tests can assert on the wire value, not the
                       human label. */
                    data-status={status}
                >
                    {p.label}
                </span>
                {rpcUrl && (
                    <code
                        className="execlaw-muted"
                        title="Loopback URL the supervisor published"
                    >
                        {rpcUrl}
                    </code>
                )}
            </div>

            {fetchError && (
                <div
                    className="execlaw-muted small mb-2"
                    data-testid={`${testidPrefix}-sidecar-fetch-error`}
                >
                    Account fetch failed: {fetchError}
                </div>
            )}

            {/* Caller-supplied hint takes precedence over our default
                explainer. Lets WhatsApp say "auto-creating wuzapi user…"
                during awaiting_pairing without us hard-coding plugin
                semantics here. */}
            {followupHint ? (
                <div className="execlaw-muted small">{followupHint}</div>
            ) : (
                p.explainer && (
                    <div className="execlaw-muted small">{p.explainer}</div>
                )
            )}
        </div>
    );
}
