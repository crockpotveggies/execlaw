// Centered "fresh chat" view — shown when no active thread is set
// or the active thread has no messages yet. Composer is the focal
// point; a small list of suggested starter prompts sits below.
//
// Sending from here mints a thread (handled by the parent's onSend
// callback) so the user never has to think about creating one
// manually.

import { useAuth } from "../auth/AuthContext";
import { useT } from "../i18n";
import { Composer } from "./Composer";
import { MascotGreeting } from "./MascotGreeting";
import { useVoiceReadiness } from "./useVoiceReadiness";

interface SuggestionDef {
    key: string;
    titleDefault: string;
    subDefault: string;
    promptDefault: string;
}

const SUGGESTIONS: ReadonlyArray<SuggestionDef> = [
    {
        key: "capabilities",
        titleDefault: "Show me what you can do",
        subDefault: "list your capabilities",
        promptDefault: "Give me a quick tour of what you can help with.",
    },
    {
        key: "plan",
        titleDefault: "Help me plan",
        subDefault: "today's priorities",
        promptDefault: "Help me figure out what's most important to do today.",
    },
    {
        key: "brainstorm",
        titleDefault: "Brainstorm",
        subDefault: "open question I've been chewing on",
        promptDefault:
            "I've been thinking about a problem at work. Can we brainstorm together?",
    },
];

import type { InlineAttachment, SkillListEntry } from "../api/endpoints";

interface Props {
    onSend: (
        text: string,
        attachments: InlineAttachment[],
        skillNames: string[],
    ) => Promise<void> | void;
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
    /**
     * 2026-05-15 — gates the composer's image-attach affordance.
     * Threaded down from the chat shell's `useBackendCapabilities`
     * probe. When undefined (e.g. tests, settings shell), defaults
     * to text-only.
     */
    multimodal?: boolean;
    /**
     * 2026-05-15 — target long-edge dimension for client-side
     * downscale. See `Composer.recommendedImageEdge`.
     */
    recommendedImageEdge?: number;
    /**
     * 2026-05-15 — lazy fetcher for the composer's "Attach skill"
     * picker. Threaded through to Composer; see its prop docs for
     * details. Optional (tests can omit; the menu item simply
     * doesn't render).
     */
    getSkills?: () => Promise<SkillListEntry[]>;
}

export function WelcomeView({
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
    onStop,
    busy,
    incognito,
    onToggleIncognito,
    multimodal,
    recommendedImageEdge,
    getSkills,
}: Props) {
    const auth = useAuth();
    const t = useT();
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
                            ? t(
                                  "welcome.incognitoOnAria",
                                  "Incognito on — turn off",
                              )
                            : t(
                                  "welcome.incognitoOffAria",
                                  "Start an incognito chat (not saved)",
                              )
                    }
                    title={
                        incognito
                            ? t(
                                  "welcome.incognitoOnTitle",
                                  "Incognito on — this chat won't be saved",
                              )
                            : t(
                                  "welcome.incognitoOffTitle",
                                  "Incognito off — start a private chat",
                              )
                    }
                    data-testid="welcome-incognito-toggle"
                >
                    <i className="bi bi-incognito" aria-hidden />
                </button>
            )}
            <div className="execlaw-welcome__brand">
                <MascotGreeting
                    size={216}
                    userName={
                        auth.user?.display_name ||
                        auth.user?.username ||
                        t("welcome.fallbackName", "friend")
                    }
                />
            </div>

            <div
                className="execlaw-welcome__composer"
                data-flip-id="composer-shell"
            >
                <Composer
                    onSend={onSend}
                    sendVoiceFrame={sendVoiceFrame}
                    sendVoiceControl={sendVoiceControl}
                    voiceReadiness={voiceReadiness}
                    onStop={onStop}
                    busy={busy}
                    multimodal={multimodal}
                    recommendedImageEdge={recommendedImageEdge}
                    getSkills={getSkills}
                />
            </div>

            <div className="execlaw-welcome__suggestions">
                <div className="execlaw-welcome__suggestions-label">
                    <i className="bi bi-lightning-charge" aria-hidden />
                    {t("welcome.suggestionsLabel", "Suggested")}
                </div>
                {SUGGESTIONS.map((s) => {
                    const title = t(
                        `welcome.suggestions.${s.key}.title`,
                        s.titleDefault,
                    );
                    const sub = t(
                        `welcome.suggestions.${s.key}.sub`,
                        s.subDefault,
                    );
                    const prompt = t(
                        `welcome.suggestions.${s.key}.prompt`,
                        s.promptDefault,
                    );
                    return (
                        <button
                            key={s.key}
                            type="button"
                            className="execlaw-welcome__suggestion"
                            data-testid="welcome-suggestion"
                            onClick={() => void onSend(prompt, [], [])}
                        >
                            <span className="execlaw-welcome__suggestion-title">
                                {title}
                            </span>
                            <span className="execlaw-welcome__suggestion-sub">
                                {sub}
                            </span>
                        </button>
                    );
                })}
            </div>
        </div>
    );
}
