// AppBoot — first render after the SPA loads.
//
// 1. Probe `/api/ping` to learn whether the server has been initialized.
// 2. While probing, render a minimal loading screen.
// 3. On `setup`   → /setup (account-creation step).
// 4. On `wizard`  → /setup (resume at docker step). Phase-14:
//                   account exists but the first-run wizard hasn't
//                   completed; route back so the operator finishes
//                   the docker + backend steps. Without this, a
//                   refresh mid-wizard left the user kicked into
//                   /chat with no inference backend.
// 5. On `pong`    → /login (or /chat if AuthProvider already restored a session).
// 6. On error     → an unobtrusive "can't reach server" panel with retry.
//
// The setup-state probe runs once per app load. After setup or login,
// we navigate forward by-route, not by re-pinging — except for the
// `RequireSetupComplete` guard, which re-probes on entering /chat or
// /settings to catch direct-URL navigation past an unfinished wizard.

import { useEffect, useState } from "react";
import { Navigate } from "react-router-dom";
import Spinner from "react-bootstrap/Spinner";
import Button from "react-bootstrap/Button";
import { ping, type PingState } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

type ProbeState =
    | { phase: "loading" }
    | { phase: "ready"; ping: PingState }
    | { phase: "error"; message: string };

export function AppBoot() {
    const auth = useAuth();
    const [probe, setProbe] = useState<ProbeState>({ phase: "loading" });

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const state = await ping();
                if (!cancelled) setProbe({ phase: "ready", ping: state });
            } catch (e) {
                if (!cancelled)
                    setProbe({
                        phase: "error",
                        message: (e as Error).message ?? "unknown error",
                    });
            }
        })();
        return () => {
            cancelled = true;
        };
    }, []);

    // While either the auth bootstrap or the ping probe is in flight,
    // sit on the loading screen.
    if (auth.status === "loading" || probe.phase === "loading") {
        return <BootLoadingScreen />;
    }

    if (probe.phase === "error") {
        return <BootErrorScreen message={probe.message} />;
    }

    // Setup-required states route to /setup regardless of auth —
    // SetupWizard handles both "no controller" (account form) and
    // "controller exists, wizard mid-flow" (resume at docker step).
    if (probe.ping === "setup" || probe.ping === "wizard") {
        return <Navigate to="/setup" replace />;
    }

    // Auth context already restored an active session → straight to chat.
    if (auth.status === "authenticated") {
        return <Navigate to="/chat" replace />;
    }

    // Server initialized, no local session → login.
    return <Navigate to="/login" replace />;
}

function BootLoadingScreen() {
    return (
        <div className="execlaw-auth-shell">
            <div className="text-center execlaw-muted">
                <Spinner
                    animation="border"
                    role="status"
                    aria-label="Connecting to execlaw"
                    size="sm"
                />
                <div className="mt-3 small">Connecting to execlaw…</div>
            </div>
        </div>
    );
}

function BootErrorScreen({ message }: { message: string }) {
    return (
        <div className="execlaw-auth-shell">
            <div className="execlaw-auth-card">
                <h1 className="execlaw-brand h4 mb-3">execlaw</h1>
                <div className="execlaw-error-banner mb-3" role="alert">
                    Can&rsquo;t reach the server.
                </div>
                <p className="execlaw-muted small mb-3" data-testid="boot-error-detail">
                    {message}
                </p>
                <Button
                    variant="primary"
                    onClick={() => window.location.reload()}
                >
                    Retry
                </Button>
            </div>
        </div>
    );
}
