// Centered "fresh chat" view — shown when no active thread is set
// or the active thread has no messages yet. Composer is the focal
// point; a small list of suggested starter prompts sits below.
//
// Sending from here mints a thread (handled by the parent's onSend
// callback) so the user never has to think about creating one
// manually.

import { Composer } from "./Composer";

const SUGGESTIONS: ReadonlyArray<{ title: string; sub: string; prompt: string }> = [
    {
        title: "Show me what you can do",
        sub: "list your capabilities",
        prompt: "Give me a quick tour of what you can help with.",
    },
    {
        title: "Help me plan",
        sub: "today's priorities",
        prompt: "Help me figure out what's most important to do today.",
    },
    {
        title: "Brainstorm",
        sub: "open question I've been chewing on",
        prompt:
            "I've been thinking about a problem at work. Can we brainstorm together?",
    },
];

interface Props {
    onSend: (text: string) => Promise<void> | void;
    /**
     * Phase 13.A — voice mic button surfaces here too so the
     * operator can start a voice conversation without typing
     * anything first. Optional; absent in tests that don't bother
     * with voice plumbing.
     */
    sendVoiceFrame?: (bytes: ArrayBuffer) => boolean;
}

export function WelcomeView({ onSend, sendVoiceFrame }: Props) {
    return (
        <div className="execlaw-welcome" data-testid="welcome-view">
            <div className="execlaw-welcome__brand">
                <i className="bi bi-stars" aria-hidden />
                <span>execlaw</span>
            </div>

            <div className="execlaw-welcome__composer">
                <Composer onSend={onSend} sendVoiceFrame={sendVoiceFrame} />
            </div>

            <div className="execlaw-welcome__suggestions">
                <div className="execlaw-welcome__suggestions-label">
                    <i className="bi bi-lightning-charge" aria-hidden />
                    Suggested
                </div>
                {SUGGESTIONS.map((s) => (
                    <button
                        key={s.title}
                        type="button"
                        className="execlaw-welcome__suggestion"
                        data-testid="welcome-suggestion"
                        onClick={() => void onSend(s.prompt)}
                    >
                        <span className="execlaw-welcome__suggestion-title">
                            {s.title}
                        </span>
                        <span className="execlaw-welcome__suggestion-sub">
                            {s.sub}
                        </span>
                    </button>
                ))}
            </div>
        </div>
    );
}
