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

interface Props {
    /**
     * Called for every captured audio chunk. Returns true when the
     * underlying WebSocket queued the bytes; false drops the chunk
     * silently. The voice pipeline's VAD tolerates short audio gaps,
     * so dropping is safer than buffering across reconnects.
     */
    sendBinary: (bytes: ArrayBuffer) => boolean;
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
    disabled,
    timesliceMs = DEFAULT_TIMESLICE_MS,
}: Props) {
    const [recording, setRecording] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const recorderRef = useRef<MediaRecorder | null>(null);
    const streamRef = useRef<MediaStream | null>(null);

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
        setRecording(false);
    }, []);

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
            stream = await navigator.mediaDevices.getUserMedia({
                audio: {
                    echoCancellation: true,
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
        recorder.ondataavailable = (ev) => {
            if (ev.data.size === 0) return;
            void ev.data.arrayBuffer().then((buf) => {
                // Drop the chunk silently if the WebSocket isn't
                // open. The VAD tolerates short gaps; trying to
                // buffer across reconnects produces stale audio.
                sendBinary(buf);
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
