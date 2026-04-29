// Phase 13.A — voice mic button + MediaRecorder capture.
//
// Click → request mic permission → start recording → stream chunks
// upstream as binary WebSocket frames. Click again → stop the
// recorder and release the mic.
//
// Scope of 13.A: just the audio-capture-and-send wire. Server-side
// processing (VAD, STT, LLM, TTS) lands in 13.B–13.E. We send chunks
// every ~250ms so a future end-of-utterance heuristic on the server
// has fine-enough granularity to detect short pauses.
//
// Browser-compat note: MediaRecorder is on every modern desktop +
// Android browser; iOS Safari needs `audio/mp4` instead of the
// default `audio/webm`. We pick the first supported MIME type so a
// single component covers both.

import { useCallback, useEffect, useRef, useState } from "react";
import Button from "react-bootstrap/Button";
import OverlayTrigger from "react-bootstrap/OverlayTrigger";
import Tooltip from "react-bootstrap/Tooltip";
import { ErrorBanner } from "../components/ErrorBanner";
import { codecFromMime, VoiceSession } from "./VoiceSession";

interface Props {
    /**
     * Called for every captured audio chunk. Returns true when the
     * underlying WebSocket queued the bytes; false drops the chunk
     * silently. The voice pipeline's VAD tolerates short audio gaps,
     * so dropping is safer than buffering across reconnects.
     *
     * `null` when the WebSocket isn't connected at all (e.g. the
     * settings shell). The button still renders in that case so
     * the operator sees the voice affordance, but it's muted with
     * a "voice unavailable" tooltip.
     */
    sendBinary: ((bytes: ArrayBuffer) => boolean) | null;
    /**
     * Phase 13.C — fire a control message upstream. Carries
     * voice_stop on mic-off (server flushes STT + runs the agent
     * reply path). Returns false silently when the WS is offline.
     */
    sendControl?: (payload: object) => boolean;
    /**
     * Disabled when the chat shell is busy with another action
     * (e.g. composer mid-submit) or when the WebSocket is offline.
     */
    disabled?: boolean;
    /**
     * Phase 14.D — voice backend readiness. When `ready: false`
     * the button renders with a muted-mic icon (line through) +
     * a tooltip explaining what's missing (STT not configured,
     * TTS warming up, etc.). Operator clicks become no-ops.
     *
     * Optional so callers that already know voice can run can
     * skip the readiness probe entirely (tests, in-memory mocks).
     */
    readiness?: { ready: boolean; tooltip: string; loading: boolean } | null;
    /// How often the recorder slices the audio stream into chunks.
    /// Defaults to 250ms — small enough that endpointer latency
    /// stays under the 300ms budget on the server side.
    timesliceMs?: number;
}

const DEFAULT_TIMESLICE_MS = 250;

const PREFERRED_MIME_TYPES = [
    "audio/webm;codecs=opus",
    "audio/webm",
    "audio/ogg;codecs=opus",
    "audio/mp4",
] as const;

function pickSupportedMimeType(): string | undefined {
    if (typeof MediaRecorder === "undefined") return undefined;
    for (const t of PREFERRED_MIME_TYPES) {
        if (MediaRecorder.isTypeSupported(t)) return t;
    }
    return undefined;
}

export function VoiceCaptureButton({
    sendBinary,
    sendControl,
    disabled,
    readiness,
    timesliceMs = DEFAULT_TIMESLICE_MS,
}: Props) {
    const [recording, setRecording] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const recorderRef = useRef<MediaRecorder | null>(null);
    const streamRef = useRef<MediaStream | null>(null);
    /// Phase 13.A closure — voice-session lifecycle. A fresh
    /// session id + seq counter is minted on every recording start;
    /// torn down on stop. `framePayload` wraps each chunk in the
    /// `[u32 header_len][JSON header][opus payload]` wire format
    /// the server-side `voice_frame::parse_frame` consumes.
    const sessionRef = useRef<VoiceSession | null>(null);

    // Releasing the mic on unmount avoids a "tab is using your mic"
    // browser indicator persisting after the component is gone.
    useEffect(() => {
        return () => {
            if (recorderRef.current && recorderRef.current.state !== "inactive") {
                try {
                    recorderRef.current.stop();
                } catch {
                    /* ignore */
                }
            }
            if (streamRef.current) {
                for (const track of streamRef.current.getTracks()) {
                    track.stop();
                }
                streamRef.current = null;
            }
        };
    }, []);

    const stopRecording = useCallback(() => {
        const r = recorderRef.current;
        if (r && r.state !== "inactive") {
            try {
                r.stop();
            } catch {
                /* ignore */
            }
        }
        recorderRef.current = null;
        if (streamRef.current) {
            for (const track of streamRef.current.getTracks()) {
                track.stop();
            }
            streamRef.current = null;
        }
        // Phase 13.C — tell the server to flush STT + run the agent
        // reply path. The server's `voice_stop` handler also closes
        // the registry session and emits VoiceSessionEnded.
        const sess = sessionRef.current;
        if (sess && sendControl) {
            sendControl({ op: "voice_stop", session: sess.sessionId });
        }
        sessionRef.current = null;
        setRecording(false);
    }, [sendControl]);

    const startRecording = useCallback(async () => {
        setError(null);
        if (typeof navigator === "undefined" || !navigator.mediaDevices?.getUserMedia) {
            setError("This browser doesn't support microphone capture.");
            return;
        }
        if (typeof MediaRecorder === "undefined") {
            setError("This browser doesn't support MediaRecorder.");
            return;
        }
        let stream: MediaStream;
        try {
            // Browser-side echo cancellation OFF — operator decision.
            // The plan's WebRTC AEC3 (Phase 13.E) needs the browser to
            // pass through the raw mic signal so the server can reason
            // about both the mic and the speaker. Browser-built-in AEC
            // would mangle the signal first. Until 13.E lands, the
            // limitation is "agent's TTS may pick up in the mic if
            // speakers are loud" — acceptable for pre-13.E dogfood.
            // NS + AGC stay on; they don't interfere with downstream
            // AEC the way browser AEC does.
            stream = await navigator.mediaDevices.getUserMedia({
                audio: {
                    echoCancellation: false,
                    noiseSuppression: true,
                    autoGainControl: true,
                },
            });
        } catch (e) {
            setError(
                e instanceof Error
                    ? `Mic permission denied: ${e.message}`
                    : "Mic permission denied.",
            );
            return;
        }
        streamRef.current = stream;
        const mimeType = pickSupportedMimeType();
        let recorder: MediaRecorder;
        try {
            recorder = mimeType
                ? new MediaRecorder(stream, { mimeType })
                : new MediaRecorder(stream);
        } catch (e) {
            // Tear down the freshly-acquired mic on construction
            // failure so the indicator dot vanishes.
            for (const t of stream.getTracks()) t.stop();
            streamRef.current = null;
            setError(
                e instanceof Error
                    ? `Couldn't start recorder: ${e.message}`
                    : "Couldn't start recorder.",
            );
            return;
        }
        recorderRef.current = recorder;
        // Phase 13.A closure — mint the voice session before the
        // recorder fires its first chunk. Sample rate comes from
        // the AudioContext default since MediaRecorder doesn't
        // expose its capture rate directly; browsers ship 48000.
        // The server-side decoder relies on this header field to
        // resample for Whisper.
        const sampleRate =
            (typeof AudioContext !== "undefined"
                ? new AudioContext().sampleRate
                : undefined) ?? 48000;
        const session = new VoiceSession({
            codec: codecFromMime(mimeType),
            sampleRate,
        });
        sessionRef.current = session;
        recorder.ondataavailable = (ev) => {
            if (ev.data.size === 0) return;
            void ev.data.arrayBuffer().then((buf) => {
                // Wrap the opus payload in the wire framing so
                // the server can parse session id + seq + codec
                // metadata without out-of-band setup. Drops the
                // chunk silently if the WebSocket isn't open —
                // VAD tolerates short gaps; buffering across
                // reconnects produces stale audio.
                const sess = sessionRef.current;
                if (!sess) return;
                // sendBinary is null when the WebSocket isn't
                // connected; the muted-mic gate above prevents
                // recording from starting in that state, but be
                // defensive — drop the chunk if we somehow ended
                // up here without a sender.
                if (sendBinary !== null) {
                    sendBinary(sess.framePayload(buf));
                }
            });
        };
        recorder.onstop = () => {
            recorderRef.current = null;
            if (streamRef.current) {
                for (const track of streamRef.current.getTracks()) {
                    track.stop();
                }
                streamRef.current = null;
            }
            sessionRef.current = null;
            setRecording(false);
        };
        try {
            recorder.start(timesliceMs);
            setRecording(true);
        } catch (e) {
            setError(
                e instanceof Error
                    ? `Couldn't start: ${e.message}`
                    : "Couldn't start.",
            );
        }
    }, [sendBinary, timesliceMs]);

    const onClick = useCallback(() => {
        if (recording) {
            stopRecording();
        } else {
            void startRecording();
        }
    }, [recording, startRecording, stopRecording]);

    // Determine the operator-visible state. Three exclusive cases:
    //   * `recording`  → red mic-fill, click stops.
    //   * `voiceUnavailable` (no WS) OR `!readiness.ready` (STT/TTS
    //     missing or warming up) → muted-mic icon (line through),
    //     click is a no-op, hover surfaces the tooltip explaining
    //     the setup state.
    //   * default → empty mic, click starts capture.
    const voiceUnavailable = sendBinary === null;
    const readinessKnown = readiness !== undefined && readiness !== null;
    // When `readiness` is omitted the caller is opting out of the
    // probe (tests, mocks); treat that as ready=true so the
    // button behaves as the pre-Phase-14.D version did.
    const readinessReady = readinessKnown ? readiness!.ready : true;
    const muted = voiceUnavailable || !readinessReady;
    const tooltipText = (() => {
        if (recording) return "Click to stop voice capture";
        if (voiceUnavailable)
            return "Voice unavailable — the chat WebSocket isn't connected.";
        if (readinessKnown && readiness!.loading)
            return "Checking voice backend status…";
        if (readinessKnown && !readiness!.ready) return readiness!.tooltip;
        return "Click to start voice. Speak when the indicator turns red.";
    })();

    const iconClass = recording
        ? "bi bi-mic-fill"
        : muted
        ? "bi bi-mic-mute"
        : "bi bi-mic";

    const button = (
        <Button
            type="button"
            variant={
                recording
                    ? "danger"
                    : muted
                    ? "outline-secondary"
                    : "outline-secondary"
            }
            onClick={muted ? undefined : onClick}
            disabled={disabled || muted}
            data-testid="composer-voice"
            data-mic-state={
                recording ? "recording" : muted ? "muted" : "ready"
            }
            aria-label={
                recording
                    ? "Stop voice capture"
                    : muted
                    ? `Voice unavailable: ${tooltipText}`
                    : "Start voice capture"
            }
            aria-pressed={recording}
        >
            <i className={iconClass} aria-hidden />
        </Button>
    );

    return (
        <>
            <OverlayTrigger
                placement="top"
                overlay={
                    <Tooltip id="composer-voice-tooltip">{tooltipText}</Tooltip>
                }
            >
                {button}
            </OverlayTrigger>
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="mt-2"
                testId="composer-voice-error"
            />
        </>
    );
}
