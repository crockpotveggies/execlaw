// Top-level banner that surfaces the live `ConnectionStatus`.
//
// `online`        → renders nothing.
// `reconnecting`  → small yellow strip: "Reconnecting to server…".
// `offline`       → red strip: "Server unreachable. Check the
//                   control plane is running."
//
// The banner is mounted at the App root so every authenticated screen
// inherits it without a per-route hook. We deliberately do NOT touch
// the router state on disconnect — the operator stays on whatever
// page they were on; only the banner changes.

import { useConnectionStatus } from "../api/connection";

export function ConnectionBanner() {
    const status = useConnectionStatus();
    if (status === "online") return null;
    const isOffline = status === "offline";
    const label = isOffline
        ? "Server unreachable. Check that the control plane is running."
        : "Reconnecting to server…";
    return (
        <div
            data-testid="connection-banner"
            data-status={status}
            role="status"
            style={{
                position: "fixed",
                top: 0,
                left: 0,
                right: 0,
                zIndex: 2000,
                padding: "6px 12px",
                fontSize: 12,
                textAlign: "center",
                background: isOffline ? "#7a1e1e" : "#7a601e",
                color: "#fff",
                pointerEvents: "none",
            }}
        >
            {label}
        </div>
    );
}
