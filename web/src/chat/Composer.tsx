// Composer — text input + send button rendered as a single Bootstrap
// input-group so they're visually flush. Auto-grows the textarea up
// to a max height; sends on Enter, Shift+Enter inserts a newline.
//
// Phase 13.A — also hosts the voice mic button to the left of the
// send button. Voice capture is gated on the chat shell providing a
// `sendVoiceFrame` accessor; when absent (e.g. settings shell) the
// button is hidden.

import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import type { VoiceReadiness } from "./useVoiceReadiness";
import { VoiceCaptureButton } from "./VoiceCaptureButton";

interface Props {
    disabled?: boolean;
    /**
     * Channel name (e.g. "signal") when this thread is bridged to a
     * non-web transport. Drops the textarea/send/mic chrome entirely
     * and renders a "Thread is managed on …" notice instead — the
     * operator's outbound on a Signal-bridged thread MUST flow
     * through Signal itself, not the web composer.
     *
     * `null` / `undefined` for normal web threads.
     */
    bridgedChannel?: string | null;
    onSend: (text: string) => Promise<void> | void;
    /**
     * Phase 13.A — voice-mode wire. Returns true when the bytes
     * were queued on the WebSocket; false drops the chunk. Pass
     * `undefined` to hide the mic button entirely (e.g. routes
     * that don't have a live WebSocket).
     */
    sendVoiceFrame?: (bytes: ArrayBuffer) => boolean;
    /// Phase 13.C — voice control message accessor. Used to fire
    /// `voice_stop` on mic-off so the server flushes Whisper. The
    /// VoiceCaptureButton calls this when its session ends.
    sendVoiceControl?: (payload: object) => boolean;
    /**
     * Phase 14.D — voice backend readiness, sourced from
     * `useVoiceReadiness()` in the parent shell (Chat /
     * WelcomeView). Threading this as a prop instead of
     * calling the hook in Composer keeps the component
     * AuthProvider-independent — tests render Composer in
     * isolation and would otherwise blow up on `useAuth()`.
     * `undefined` means the caller hasn't probed; the mic
     * button defaults to "ready" so existing test setups keep
     * working.
     */
    voiceReadiness?: VoiceReadiness | null;
    /**
     * 2026-04-28 — true while a turn is streaming server-side. When
     * set, the send button is replaced by a stop button that calls
     * `onStop`. Parent shell tracks streaming state from the
     * WebSocket `phase=thinking|awaiting_tool` events. Optional —
     * routes without streaming (e.g. settings) leave it undefined
     * and the stop button never appears.
     */
    busy?: boolean;
    /**
     * 2026-04-28 — invoked when the operator clicks the stop button.
     * Parent calls `postStopTurn(conversationId)`. Optional; when
     * absent the stop button is hidden even if `busy` is true.
     */
    onStop?: () => void;
}

export function Composer({
    disabled,
    bridgedChannel,
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
    voiceReadiness,
    busy,
    onStop,
}: Props) {
    // Bridged-channel short-circuit. Render a flat, non-interactive
    // notice in place of the composer chip. Mirrors the chip's
    // outer dimensions (max-width, padding, border-radius via the
    // shared `.execlaw-composer__shell` class) so swapping back to
    // the live composer when the operator opens an unbridged thread
    // doesn't reflow the chat region.
    if (bridgedChannel) {
        const channelLabel =
            bridgedChannel.charAt(0).toUpperCase() + bridgedChannel.slice(1);
        return (
            <div
                className="execlaw-composer__shell execlaw-composer__shell--bridged"
                data-testid="composer-bridged-notice"
                data-channel={bridgedChannel}
                role="status"
                aria-live="polite"
                style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    color: "#7d8590",
                    fontSize: "0.875rem",
                    padding: "0.85rem 1rem",
                    cursor: "not-allowed",
                }}
            >
                <i className="bi bi-info-circle" aria-hidden style={{ marginRight: "0.5rem" }} />
                Thread is managed on {channelLabel}
            </div>
        );
    }
    const [text, setText] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const textareaRef = useRef<HTMLTextAreaElement | null>(null);

    // Auto-grow.
    useEffect(() => {
        const ta = textareaRef.current;
        if (!ta) return;
        ta.style.height = "auto";
        ta.style.height = `${Math.min(ta.scrollHeight, 192)}px`;
    }, [text]);

    const submit = async (e?: FormEvent | KeyboardEvent) => {
        e?.preventDefault();
        const trimmed = text.trim();
        if (trimmed.length === 0 || submitting) return;
        setSubmitting(true);
        // 2026-04-28 — clear the textarea synchronously, BEFORE
        // awaiting `onSend`. With local-model streaming, `onSend`
        // (= postMessage) doesn't resolve until the agent's full
        // reply has finished generating — so a post-await clear
        // left the user's text sitting in the input for tens of
        // seconds, looking like the send button hadn't worked. The
        // optimistic message already lands in the transcript via
        // `appendMessage` inside onSend, so clearing eagerly is
        // safe even if the server later errors (the operator can
        // re-type from the rejected message in the transcript).
        setText("");
        try {
            await onSend(trimmed);
        } finally {
            setSubmitting(false);
        }
    };

    const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
        if (e.key === "Enter" && !e.shiftKey) {
            void submit(e);
        }
    };

    const isBusy = !!submitting || !!disabled;
    const trimmedEmpty = text.trim().length === 0;

    // ChatGPT-style composer:
    //   * The visible "chip" is the outer `<div>`. It owns the
    //     background fill, the rounded corners, and the focus-within
    //     ring. The textarea inside is fully transparent so it
    //     reads as part of the chip rather than a separate input.
    //   * Tool affordances (mic, future attach / web-search /
    //     model-picker, etc.) anchor on a bottom row inside the
    //     same chip — left-aligned tools, right-aligned send.
    //
    // We hand-roll the layout instead of using Bootstrap's
    // InputGroup because InputGroup is fundamentally a single
    // horizontal row (textarea + buttons inline) and we want a
    // two-row chip (textarea on top, tools below).
    return (
        <form onSubmit={submit} className="execlaw-composer__form">
            <div className="execlaw-composer__shell" data-testid="composer-shell">
                <Form.Control
                    ref={textareaRef}
                    as="textarea"
                    rows={1}
                    placeholder="How can I help?"
                    value={text}
                    onChange={(e) => setText(e.target.value)}
                    onKeyDown={onKeyDown}
                    // 2026-04-28 — only honour the *external* disabled
                    // prop. Previously we also disabled while the
                    // submit await was in flight, which (a) blurred
                    // the textarea (disabled inputs can't hold
                    // focus) and (b) blocked the operator from
                    // composing a follow-up while the agent was
                    // streaming. The double-send guard lives in
                    // `submit()` (`if (submitting) return`); the
                    // send button itself stays disabled (see
                    // `isBusy` below). Keeping the textarea hot
                    // means focus survives Enter and the operator
                    // can queue their next thought immediately.
                    disabled={!!disabled}
                    className="execlaw-composer__input"
                    data-testid="composer-input"
                />
                <div className="execlaw-composer__tools">
                    <div className="execlaw-composer__tools-left">
                        {/*
                          Reserved for future attach / web-search /
                          tool-picker affordances. Keeping the slot
                          empty (rather than absent) preserves the
                          flex `space-between` alignment so the
                          send button stays right-anchored even
                          before tools land.
                        */}
                    </div>
                    <div className="execlaw-composer__tools-right">
                        {/*
                          Mic always renders so the operator sees
                          the affordance even when:
                            * the websocket isn't connected
                              (sendVoiceFrame undefined → null), OR
                            * voice backends aren't configured /
                              healthy yet.
                          The button itself owns the muted-vs-ready
                          state + the tooltip; we just feed it the
                          two relevant signals.
                        */}
                        <VoiceCaptureButton
                            sendBinary={sendVoiceFrame ?? null}
                            sendControl={sendVoiceControl}
                            disabled={isBusy}
                            readiness={voiceReadiness ?? null}
                        />
                        {(submitting || busy) && onStop ? (
                            // 2026-04-28 — replace the send button with a
                            // stop button while a turn is streaming. The
                            // local-model agent can otherwise generate
                            // for minutes with no escape hatch; this
                            // gives the operator the same "stop"
                            // affordance every chat UI ships.
                            //
                            // Triggered by EITHER:
                            //   * `submitting` — the await on `onSend`
                            //     hasn't resolved yet; postMessage
                            //     only returns after the server
                            //     finishes the turn, so this stays
                            //     true for the whole streaming
                            //     window. This is the most reliable
                            //     signal because it doesn't depend
                            //     on any WS phase event landing in
                            //     time.
                            //   * `busy` — external phase signal
                            //     (e.g. a routine fired the turn
                            //     so this tab didn't initiate it).
                            <Button
                                type="button"
                                variant="primary"
                                onClick={(ev) => {
                                    ev.preventDefault();
                                    onStop();
                                }}
                                data-testid="composer-stop"
                                aria-label="Stop generating"
                                className="execlaw-composer__send execlaw-composer__send--stop"
                            >
                                <i className="bi bi-stop-fill" aria-hidden />
                            </Button>
                        ) : (
                            <Button
                                type="submit"
                                variant="primary"
                                disabled={isBusy || trimmedEmpty}
                                data-testid="composer-send"
                                aria-label="Send"
                                className="execlaw-composer__send"
                            >
                                {submitting ? (
                                    <Spinner size="sm" animation="border" />
                                ) : (
                                    <i className="bi bi-arrow-up" aria-hidden />
                                )}
                            </Button>
                        )}
                    </div>
                </div>
            </div>
        </form>
    );
}
