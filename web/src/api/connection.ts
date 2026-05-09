// Live connection-health surface for the SPA.
//
// Tracks two independent signals and rolls them up to a single status
// the UI can render:
//
//   * REST: every `apiFetch` reports its outcome — `success` resets
//     the counter, `network` (TypeError from fetch / DNS / CORS / dev
//     server gone) bumps a sticky "rest unreachable" flag.
//   * WS: `WsClient` emits status transitions on open / close /
//     reconnect-pending. A close that's followed by a successful
//     reopen is invisible to the UI; sustained close or backoff
//     escalation surfaces.
//
// The combined `ConnectionStatus`:
//
//   * `online`     — both REST + WS healthy.
//   * `reconnecting` — WS dropped within the last few seconds; REST
//                      may still be fine. Yellow banner.
//   * `offline`    — REST has been failing (network errors) AND WS
//                    has been disconnected for more than the warmup
//                    window. Red banner, "Server unreachable".
//
// The store is a plain module-scoped pub/sub so non-React modules
// (apiFetch, WsClient) can publish without dragging in React. The
// `useConnectionStatus()` hook subscribes from any component.

import { useEffect, useState } from "react";

/**
 * `idle` — no WS client currently expects to be connected (boot,
 *          login screen, after a deliberate `close()`). Rolls up
 *          to `online` because there's no problem to report.
 * `open` — WS is up.
 * `reconnecting` — WS wants to be connected but isn't right now;
 *          auto-reconnect is armed.
 */
export type WsConnectionState = "idle" | "open" | "reconnecting";

export type ConnectionStatus = "online" | "reconnecting" | "offline";

interface SnapshotShape {
    ws: WsConnectionState;
    /** Wall-clock ms of the most recent REST network failure, or 0. */
    lastRestNetworkErrorAt: number;
    /** Wall-clock ms of the most recent REST success, or 0. */
    lastRestSuccessAt: number;
}

const REST_OFFLINE_GRACE_MS = 4_000;

let snapshot: SnapshotShape = {
    ws: "idle",
    lastRestNetworkErrorAt: 0,
    lastRestSuccessAt: 0,
};

type Listener = (status: ConnectionStatus, snap: SnapshotShape) => void;
const listeners = new Set<Listener>();

function rollUp(snap: SnapshotShape): ConnectionStatus {
    const now = Date.now();
    const restRecentlyFailed =
        snap.lastRestNetworkErrorAt > 0 &&
        snap.lastRestNetworkErrorAt > snap.lastRestSuccessAt &&
        now - snap.lastRestNetworkErrorAt < REST_OFFLINE_GRACE_MS;
    // `idle` (no active client) + healthy REST → `online`. We don't
    // surface anything when the SPA isn't actively trying to keep a
    // socket up.
    if (snap.ws === "idle" && !restRecentlyFailed) return "online";
    if (snap.ws === "open" && !restRecentlyFailed) return "online";
    if (snap.ws === "reconnecting" && !restRecentlyFailed) return "reconnecting";
    // REST is failing. WS state escalates the severity: if we're
    // also disconnected on the WS side, assume the whole control
    // plane is down (red banner). Otherwise it's a transient REST
    // hiccup and the yellow banner is enough.
    if (restRecentlyFailed) {
        return snap.ws === "open" ? "reconnecting" : "offline";
    }
    return "online";
}

function publish(): void {
    const status = rollUp(snapshot);
    listeners.forEach((l) => {
        try {
            l(status, snapshot);
        } catch {
            /* listeners must not crash the publisher */
        }
    });
}

/** WS publisher — called from `WsClient`. */
export function reportWsState(next: WsConnectionState): void {
    if (snapshot.ws === next) return;
    snapshot = { ...snapshot, ws: next };
    publish();
}

/** REST publisher — called from `apiFetch`. */
export function reportRestNetworkError(): void {
    snapshot = { ...snapshot, lastRestNetworkErrorAt: Date.now() };
    publish();
}

/** REST publisher — every 2xx call. Resets the offline grace window. */
export function reportRestSuccess(): void {
    snapshot = { ...snapshot, lastRestSuccessAt: Date.now() };
    publish();
}

/** Snapshot accessor for code paths that don't want a subscription. */
export function getConnectionStatus(): ConnectionStatus {
    return rollUp(snapshot);
}

/** Test-only: reset the global so vitest doesn't carry state across cases. */
export function _resetConnectionState(): void {
    snapshot = {
        ws: "idle",
        lastRestNetworkErrorAt: 0,
        lastRestSuccessAt: 0,
    };
    publish();
}

/** React hook: re-render on every connection-status transition. */
export function useConnectionStatus(): ConnectionStatus {
    const [status, setStatus] = useState<ConnectionStatus>(() =>
        rollUp(snapshot),
    );
    useEffect(() => {
        const listener: Listener = (next) => setStatus(next);
        listeners.add(listener);
        // Refresh once on mount so a status change that happened
        // before subscription is reflected. Also re-roll on a timer
        // because the offline grace window expires by wall-clock,
        // not by event.
        setStatus(rollUp(snapshot));
        const tick = setInterval(() => {
            setStatus(rollUp(snapshot));
        }, 1_000);
        return () => {
            listeners.delete(listener);
            clearInterval(tick);
        };
    }, []);
    return status;
}
