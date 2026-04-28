// Phase 14.D — voice-mode readiness probe.
//
// The composer's mic button needs to know whether the operator
// has both VoiceSTT and VoiceTTS backends configured AND running
// (when managed). Without that we render a muted-mic icon + a
// tooltip telling the operator how to fix it, instead of letting
// them click into a "session ends instantly because the server
// has no STT" failure mode.
//
// Probe semantics:
//   * external rows count as ready when configured — we don't
//     reach across to probe an arbitrary URL from the SPA. The
//     server-side runner will surface the failure as an alert
//     if the URL is wrong.
//   * managed rows count as ready when configured AND status
//     reports `Healthy`. Pulling/Starting/CrashLooping → not
//     ready (with distinct tooltip copy so the operator can tell
//     "still warming up" from "broken").
//
// Polled every 30 s and re-fetched on focus so a backend that
// finishes warming up while the operator is staring at the chat
// shell un-mutes the mic without a manual refresh.

import { useCallback, useEffect, useRef, useState } from "react";
import {
    getBackendStatus,
    listBackends,
    type BackendListEntry,
    type BackendMode,
    type BackendStage,
    type BackendStatus,
} from "../api/endpoints";

/// Per-purpose readiness summary. `tooltipFragment` is the short
/// phrase the composite tooltip splices in when the purpose is
/// the blocker (e.g. "STT backend not configured").
export interface VoicePurposeReadiness {
    purpose: "VoiceSTT" | "VoiceTTS";
    configured: boolean;
    mode: BackendMode | null;
    /// `null` for external (we don't probe URLs from the SPA),
    /// `null` while the live status is in flight, otherwise the
    /// supervisor's reported status.
    runtimeStatus: BackendStatus | null;
    /// More-specific lifecycle stage from the supervisor — used
    /// when we want to render "Loading model" / "Pulling image"
    /// in the tooltip.
    stage: BackendStage | null;
    ready: boolean;
    blocker: string | null;
}

export interface VoiceReadiness {
    /// True when both VoiceSTT and VoiceTTS are configured AND
    /// — if managed — Healthy. Drives whether the mic icon
    /// renders enabled or muted.
    ready: boolean;
    /// Human-readable tooltip text. Always populated; on the
    /// happy path it explains "click to start voice", on the
    /// sad path it lists what's missing + a hint to Settings →
    /// Backends.
    tooltip: string;
    /// Per-purpose detail; the tooltip is built from these.
    stt: VoicePurposeReadiness;
    tts: VoicePurposeReadiness;
    /// True while the FIRST fetch is in flight. Lets the button
    /// stay un-styled (default state) until we've talked to the
    /// server at least once instead of flashing "not configured"
    /// for half a second on every page mount.
    loading: boolean;
}

const INITIAL_PURPOSE: VoicePurposeReadiness = {
    purpose: "VoiceSTT",
    configured: false,
    mode: null,
    runtimeStatus: null,
    stage: null,
    ready: false,
    blocker: null,
};

const INITIAL: VoiceReadiness = {
    ready: false,
    tooltip: "Voice mode availability is being checked…",
    stt: { ...INITIAL_PURPOSE, purpose: "VoiceSTT" },
    tts: { ...INITIAL_PURPOSE, purpose: "VoiceTTS" },
    loading: true,
};

/// 30s steady-state interval. The supervisor reconciles every
/// 5 s but voice readiness is operator-facing chrome — a 30 s
/// poll keeps the network footprint cheap while still
/// auto-recovering when a backend finishes loading.
const POLL_INTERVAL_MS = 30_000;

export function useVoiceReadiness(
    getToken: () => string | null,
): VoiceReadiness {
    const [state, setState] = useState<VoiceReadiness>(INITIAL);
    const aliveRef = useRef(true);

    const compute = useCallback(async () => {
        try {
            const list = await listBackends(getToken);
            const stt = list.backends.find((b) => b.purpose === "VoiceSTT");
            const tts = list.backends.find((b) => b.purpose === "VoiceTTS");
            const [sttReadiness, ttsReadiness] = await Promise.all([
                evaluatePurpose("VoiceSTT", stt, getToken),
                evaluatePurpose("VoiceTTS", tts, getToken),
            ]);
            if (!aliveRef.current) return;
            const ready = sttReadiness.ready && ttsReadiness.ready;
            const tooltip = buildTooltip(sttReadiness, ttsReadiness);
            setState({
                ready,
                tooltip,
                stt: sttReadiness,
                tts: ttsReadiness,
                loading: false,
            });
        } catch (e) {
            if (!aliveRef.current) return;
            // A network blip shouldn't unmute the mic — keep the
            // last-known state but mark loading: false so the
            // button stops showing the initial probing tooltip
            // forever. The next interval tick will retry.
            setState((prev) => ({
                ...prev,
                loading: false,
                tooltip:
                    prev.ready
                        ? prev.tooltip
                        : `Couldn't check voice backend status: ${
                              e instanceof Error ? e.message : String(e)
                          }`,
            }));
        }
    }, [getToken]);

    useEffect(() => {
        aliveRef.current = true;
        void compute();
        const id = window.setInterval(() => void compute(), POLL_INTERVAL_MS);
        const onFocus = () => void compute();
        window.addEventListener("focus", onFocus);
        return () => {
            aliveRef.current = false;
            window.clearInterval(id);
            window.removeEventListener("focus", onFocus);
        };
    }, [compute]);

    return state;
}

async function evaluatePurpose(
    purpose: "VoiceSTT" | "VoiceTTS",
    entry: BackendListEntry | undefined,
    getToken: () => string | null,
): Promise<VoicePurposeReadiness> {
    if (!entry || !entry.configured || !entry.backend) {
        return {
            purpose,
            configured: false,
            mode: null,
            runtimeStatus: null,
            stage: null,
            ready: false,
            blocker: `${purpose} backend not configured`,
        };
    }
    if (entry.backend.mode === "external") {
        // We trust the operator's URL until the runner reports
        // otherwise; can't probe an arbitrary remote endpoint
        // from the SPA without CORS pain.
        return {
            purpose,
            configured: true,
            mode: "external",
            runtimeStatus: null,
            stage: null,
            ready: true,
            blocker: null,
        };
    }
    // Managed — fetch live status. The supervisor reports
    // Healthy only when /health succeeded recently.
    try {
        const status = await getBackendStatus(purpose, getToken);
        const ready = status.status === "Healthy";
        return {
            purpose,
            configured: true,
            mode: "managed",
            runtimeStatus: status.status,
            stage: status.stage,
            ready,
            blocker: ready
                ? null
                : describeNonHealthy(purpose, status.stage, status.status),
        };
    } catch {
        return {
            purpose,
            configured: true,
            mode: "managed",
            runtimeStatus: null,
            stage: null,
            ready: false,
            blocker: `${purpose} status unavailable`,
        };
    }
}

function describeNonHealthy(
    purpose: "VoiceSTT" | "VoiceTTS",
    stage: BackendStage | null,
    status: BackendStatus | null,
): string {
    const label = purpose === "VoiceSTT" ? "Speech-to-text" : "Text-to-speech";
    switch (stage) {
        case "DownloadingModel":
            return `${label} is downloading model weights — voice will be available shortly.`;
        case "PullingImage":
            return `${label} is pulling its container image — voice will be available shortly.`;
        case "ContainerStarting":
        case "LoadingModel":
            return `${label} is warming up — voice will be available shortly.`;
        case "Failed":
            return `${label} failed to start. Check Settings → Backends for the error.`;
        case "Idle":
            return `${label} container isn't running. Try Settings → Backends → Restart.`;
        default:
            // Fallback to legacy status string when stage is not
            // populated (older server build).
            if (status === "CrashLooping")
                return `${label} is crash-looping. Check Settings → Backends → Logs.`;
            return `${label} isn't healthy yet (${status ?? "unknown"}).`;
    }
}

function buildTooltip(
    stt: VoicePurposeReadiness,
    tts: VoicePurposeReadiness,
): string {
    if (stt.ready && tts.ready) {
        return "Click to start voice. Speak when the indicator turns red.";
    }
    const blockers = [stt.blocker, tts.blocker].filter(
        (b): b is string => !!b,
    );
    if (blockers.length === 0) {
        return "Voice unavailable.";
    }
    const hint =
        !stt.configured || !tts.configured
            ? " Configure under Settings → Backends."
            : "";
    return `${blockers.join(" ")}${hint}`;
}
