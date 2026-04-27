// Phase 13.C — voice transcript banner + Interrupt button.
//
// Mounts in both WelcomeView and ActiveThreadPane so the operator
// sees the same indicator regardless of conversation state. An
// empty final transcript (server returned VoiceTranscript with
// empty text) renders an explicit "VoiceSTT didn't return a
// transcript" message — this is the SPA's only signal for the
// documented Opus → PCM codec gap (docs/voice-followups.md).

export interface VoiceTranscriptState {
    session: string;
    text: string;
    is_final: boolean;
}

export interface VoiceStatusBarProps {
    transcript: VoiceTranscriptState | null;
    sendVoiceControl: (payload: object) => boolean;
}

export function VoiceStatusBar({
    transcript,
    sendVoiceControl,
}: VoiceStatusBarProps) {
    if (!transcript) return null;
    const onInterrupt = () => {
        sendVoiceControl({
            op: "voice_interrupt",
            session: transcript.session,
        });
    };
    // Empty final transcript = either silence or the codec mismatch
    // documented in docs/voice-followups.md. Surface it explicitly
    // so the operator doesn't see a phantom blank banner.
    const isEmptyFinal =
        transcript.is_final && transcript.text.trim().length === 0;
    return (
        <div
            className="execlaw-voice-banner mx-3 mt-2 d-flex align-items-center gap-2"
            data-testid="voice-status-bar"
            data-empty-final={isEmptyFinal ? "true" : "false"}
        >
            <span className="execlaw-muted small flex-grow-1">
                {isEmptyFinal ? (
                    <>
                        <i
                            className="bi bi-exclamation-triangle me-1"
                            aria-hidden
                        />
                        Voice STT didn't return a transcript — make sure a
                        VoiceSTT backend is configured (Settings → Backends)
                        and that mic input is reaching the server.
                    </>
                ) : transcript.is_final ? (
                    <>
                        <i className="bi bi-mic me-1" aria-hidden />
                        <em>{transcript.text}</em>
                    </>
                ) : (
                    <>
                        <i className="bi bi-mic-fill me-1" aria-hidden />
                        Listening… {transcript.text}
                    </>
                )}
            </span>
            {transcript.is_final && !isEmptyFinal && (
                <button
                    type="button"
                    className="btn btn-sm btn-outline-warning"
                    onClick={onInterrupt}
                    data-testid="voice-interrupt"
                >
                    Interrupt
                </button>
            )}
        </div>
    );
}
