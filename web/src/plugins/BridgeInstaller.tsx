/**
 * Headless component that installs the host bridge
 * (`globalThis.execlawHost`) at SPA boot.
 *
 * Lives at the top of the tree (just inside `<AuthProvider>`) so
 * `useAuth().getAccessToken` is available; mounts before any route
 * that might render a `<DynamicPluginPanel>`. Renders nothing.
 *
 * The installer is idempotent — see `installHostBridge` — so HMR
 * re-renders during development don't tear down a plugin panel's
 * captured bridge reference. The mount runs once at first render
 * (via `useEffect` with empty deps + the idempotency guard).
 */

import { useEffect } from "react";
import { useAuth } from "../auth/AuthContext";
import { installHostBridge } from "./host-bridge";

export function BridgeInstaller(): null {
    const { getAccessToken } = useAuth();

    useEffect(() => {
        installHostBridge(getAccessToken);
    }, [getAccessToken]);

    return null;
}
