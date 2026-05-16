import { useEffect, useState } from "react";
import {
    BACKEND_PURPOSES,
    getBackendStatus,
    type BackendStatusResponse,
} from "../api/endpoints";

// Hook backing the sidebar brand indicator's "installing" state.
//
// Polls `/api/admin/backends/{purpose}/status` for every purpose on
// a 5-second cadence and returns `true` whenever ANY response
// matches the "an install / warm-up is in flight" signal:
//
//   * `status` ∈ {Pulling, Starting}
//   * OR `stage`  ∈ {DownloadingModel, PullingImage, ContainerStarting,
//                    LoadingModel}
//
// The two signals overlap but cover different supervisor phases
// (status === "Starting" + stage === "LoadingModel" is the common
// "model loading to GPU" case). Anything else collapses to `false`,
// including external rows (which the supervisor reports as
// status === "Stopped" with stage === "Idle").
//
// 5 seconds was picked over the plan's 2-second baseline because:
//   - The brand indicator's transitions are coarse (an install
//     takes minutes; a 5s lag before the icon switches is invisible).
//   - The backend status endpoint touches the supervisor's mutex
//     and tail_logs once per call; quadrupling that load (4
//     purposes × 2s) on every operator session was the wrong cost
//     for an indicator UX.
//
// Authentication failures and network errors are silently treated
// as "not installing" — same logic as the alert-count poll. A
// real connection break shows the disconnected icon (precedence
// `alert > disconnected > installing > ok` is enforced in the
// consumer, not here).
export function useAnyBackendInstalling(
    getToken: () => string | null,
    enabled: boolean,
): boolean {
    const [installing, setInstalling] = useState(false);

    useEffect(() => {
        if (!enabled) {
            setInstalling(false);
            return;
        }

        let cancelled = false;

        async function poll() {
            try {
                const results = await Promise.all(
                    BACKEND_PURPOSES.map((p) =>
                        getBackendStatus(p, getToken).catch(
                            () => null as BackendStatusResponse | null,
                        ),
                    ),
                );
                if (cancelled) return;
                const anyInstalling = results.some(
                    (r) =>
                        !!r &&
                        r.supervisor_available &&
                        (r.status === "Pulling" ||
                            r.status === "Starting" ||
                            r.stage === "DownloadingModel" ||
                            r.stage === "PullingImage" ||
                            r.stage === "ContainerStarting" ||
                            r.stage === "LoadingModel"),
                );
                setInstalling(anyInstalling);
            } catch {
                // Whole-promise failure (shouldn't happen with the
                // per-call `.catch` above, but defensive against
                // future refactors). Treat as "not installing".
                if (!cancelled) setInstalling(false);
            }
        }

        // Kick a poll immediately so the indicator flips within
        // a few hundred ms of the operator landing on an
        // installing system — not after a full interval tick.
        void poll();
        const id = window.setInterval(poll, 5_000);

        return () => {
            cancelled = true;
            window.clearInterval(id);
        };
    }, [getToken, enabled]);

    return installing;
}
