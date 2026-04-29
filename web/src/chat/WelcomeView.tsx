// Centered "fresh chat" view — shown when no active thread is set
// or the active thread has no messages yet. Composer is the focal
// point; a small list of suggested starter prompts sits below.
//
// Sending from here mints a thread (handled by the parent's onSend
// callback) so the user never has to think about creating one
// manually.

import { useAuth } from "../auth/AuthContext";
import { Composer } from "./Composer";
import { useVoiceReadiness } from "./useVoiceReadiness";

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
    /// Phase 13.C — voice control passthrough.
    sendVoiceControl?: (payload: object) => boolean;
    /**
     * 2026-04-28 — stop-turn handler threaded through to the inner
     * Composer. Optional because the welcome view doesn't know the
     * conversation_id until the parent mints one; the parent passes
     * a closure that captures whatever id the welcome `onSend`
     * minted.
     */
    onStop?: () => void;
    /**
     * 2026-04-28 — true while a `postMessage` is in flight from this
     * tab. Drives the Composer to swap the send button for a stop
     * button. Read from the chat store by the parent so the flag
     * survives WelcomeView ↔ ActiveThreadPane remounts.
     */
    busy?: boolean;
    /**
     * 2026-04-28 — incognito mode toggle. Lifted to the parent so
     * the next-`onSend` knows whether to mint a regular thread
     * (DB-backed) or a client-only incognito session.
     */
    incognito?: boolean;
    onToggleIncognito?: () => void;
}

export function WelcomeView({
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
    onStop,
    busy,
    incognito,
    onToggleIncognito,
}: Props) {
    const auth = useAuth();
    // Stable accessor — passing a fresh `() => auth.getAccessToken()`
    // arrow on every render used to force `useVoiceReadiness` to
    // re-fire its `useEffect` on every render, which spun a loop of
    // `listBackends` calls (1000+/sec on localhost) every time the
    // user navigated back to /chat from a settings page. The hook
    // itself is now defended via a token ref, but a stable accessor
    // here keeps the contract right at the call site.
    const getToken = auth.getAccessToken;
    const voiceReadiness = useVoiceReadiness(getToken);
    return (
        <div className="execlaw-welcome" data-testid="welcome-view">
            {/*
              Top-right incognito toggle. When OFF we render an
              outline icon; when ON the icon inverts (filled circle
              background, light icon) to clearly signal that this
              chat won't be saved. The button is always present
              even when `onToggleIncognito` is missing so tests
              that don't pass the prop don't have to mock the
              affordance — it just stays inert in that case.
            */}
            {onToggleIncognito && (
                <button
                    type="button"
                    className={
                        "execlaw-welcome__incognito" +
                        (incognito ? " is-on" : "")
                    }
                    onClick={onToggleIncognito}
                    aria-pressed={!!incognito}
                    aria-label={
                        incognito
                            ? "Incognito on — turn off"
                            : "Start an incognito chat (not saved)"
                    }
                    title={
                        incognito
                            ? "Incognito on — this chat won't be saved"
                            : "Incognito off — start a private chat"
                    }
                    data-testid="welcome-incognito-toggle"
                >
                    <i className="bi bi-incognito" aria-hidden />
                </button>
            )}
            <div className="execlaw-welcome__brand">
                <i className="bi bi-stars" aria-hidden />
                <span>execlaw</span>
            </div>

            <div className="execlaw-welcome__composer">
                <Composer
                    onSend={onSend}
                    sendVoiceFrame={sendVoiceFrame}
                    sendVoiceControl={sendVoiceControl}
                    voiceReadiness={voiceReadiness}
                    onStop={onStop}
                    busy={busy}
                />
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
