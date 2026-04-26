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
import InputGroup from "react-bootstrap/InputGroup";
import Spinner from "react-bootstrap/Spinner";
import { VoiceCaptureButton } from "./VoiceCaptureButton";

interface Props {
    disabled?: boolean;
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
}

export function Composer({
    disabled,
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
}: Props) {
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
        try {
            await onSend(trimmed);
            setText("");
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

    return (
        <form onSubmit={submit} className="execlaw-composer__form">
            <InputGroup className="execlaw-composer__group">
                <Form.Control
                    ref={textareaRef}
                    as="textarea"
                    rows={1}
                    placeholder="Type a message — Enter to send, Shift+Enter for a new line."
                    value={text}
                    onChange={(e) => setText(e.target.value)}
                    onKeyDown={onKeyDown}
                    disabled={isBusy}
                    className="execlaw-composer__input"
                    data-testid="composer-input"
                />
                {sendVoiceFrame && (
                    <VoiceCaptureButton
                        sendBinary={sendVoiceFrame}
                        sendControl={sendVoiceControl}
                        disabled={isBusy}
                    />
                )}
                <Button
                    type="submit"
                    variant="primary"
                    disabled={isBusy || trimmedEmpty}
                    data-testid="composer-send"
                    aria-label="Send"
                >
                    {submitting ? (
                        <Spinner size="sm" animation="border" />
                    ) : (
                        <i className="bi bi-send" aria-hidden />
                    )}
                </Button>
            </InputGroup>
        </form>
    );
}
