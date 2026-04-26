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
import { codecFromMime, VoiceSession } from "./VoiceSession";

interface Props {
    /**
     * Called for every captured audio chunk. Returns true when the
     * underlying WebSocket queued the bytes; false drops the chunk
     * silently. The voice pipeline's VAD tolerates short audio gaps,
     * so dropping is safer than buffering across reconnects.
     */
    sendBinary: (bytes: ArrayBuffer) => boolean;
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
                sendBinary(sess.framePayload(buf));
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

    return (
        <>
            <Button
                type="button"
                variant={recording ? "danger" : "outline-secondary"}
                onClick={onClick}
                disabled={disabled}
                data-testid="composer-voice"
                aria-label={recording ? "Stop voice capture" : "Start voice capture"}
                aria-pressed={recording}
                title={recording ? "Stop voice capture" : "Start voice capture"}
            >
                <i
                    className={recording ? "bi bi-mic-fill" : "bi bi-mic"}
                    aria-hidden
                />
            </Button>
            {error && (
                <div
                    className="execlaw-error-banner mt-2"
                    role="alert"
                    data-testid="composer-voice-error"
                >
                    {error}
                </div>
            )}
        </>
    );
}
