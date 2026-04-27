// Phase 14 — first-run-wizard guard.
//
// AppBoot routes /api/ping at first SPA load, but a deep link to
// /chat or /settings/* skips AppBoot entirely. Without this guard
// an operator who closes the wizard mid-flow and types
// http://host:3031/chat into the URL bar lands in the chat shell
// without a backend configured — the bug the user reported.
//
// The guard re-probes /api/ping on mount and:
//
//   * "setup"  → redirect to /setup (account form)
//   * "wizard" → redirect to /setup (resume at docker step)
//   * "pong"   → render the guarded route
//   * error    → render the guarded route (network blip shouldn't
//                trap the operator; the inner page may have its
//                own retry UX)
//
// Usage: wrap the route element. `App.tsx` does this for /chat and
// /settings/*.

import { useEffect, useState, type ReactNode } from "react";
import { Navigate } from "react-router-dom";
import { ping, type PingState } from "../api/endpoints";

type ProbeState =
    | { phase: "probing" }
    | { phase: "ready"; state: PingState }
    | { phase: "error" };

export function RequireSetupComplete({ children }: { children: ReactNode }) {
    const [probe, setProbe] = useState<ProbeState>({ phase: "probing" });

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const s = await ping();
                if (!cancelled) setProbe({ phase: "ready", state: s });
            } catch {
                if (!cancelled) setProbe({ phase: "error" });
            }
        })();
        return () => {
            cancelled = true;
        };
    }, []);

    // While probing, render nothing rather than the chat shell —
    // a momentary chat flash before the redirect would feel laggy.
    // Inside the SPA's own loading shell looks fine but adding a
    // dependency on the auth-shell styles for a single null guard
    // is overkill; the gap is sub-200ms in practice.
    if (probe.phase === "probing") return null;

    if (probe.phase === "ready") {
        if (probe.state === "setup" || probe.state === "wizard") {
            return <Navigate to="/setup" replace />;
        }
    }

    // "pong" or error → let the wrapped page render. Errors fall
    // through deliberately so a transient network failure doesn't
    // bounce the operator to /setup unjustly; the chat shell has
    // its own connection-error banner via the WS path.
    return <>{children}</>;
}
