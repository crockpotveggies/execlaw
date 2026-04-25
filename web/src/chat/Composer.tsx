// Composer at the bottom of the active thread. Auto-grows up to a
// max height, sends on Enter (Shift+Enter inserts a newline), shows a
// disabled state while a message is in flight.

import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import Spinner from "react-bootstrap/Spinner";

interface Props {
    disabled?: boolean;
    onSend: (text: string) => Promise<void> | void;
}

export function Composer({ disabled, onSend }: Props) {
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

    return (
        <form className="execlaw-composer__form" onSubmit={submit}>
            <textarea
                ref={textareaRef}
                className="execlaw-composer__input"
                placeholder="Type a message — Enter to send, Shift+Enter for a new line."
                value={text}
                onChange={(e) => setText(e.target.value)}
                onKeyDown={onKeyDown}
                disabled={isBusy}
                rows={1}
                data-testid="composer-input"
            />
            <button
                type="submit"
                className="btn btn-primary"
                disabled={isBusy || text.trim().length === 0}
                data-testid="composer-send"
            >
                {submitting ? (
                    <Spinner size="sm" animation="border" />
                ) : (
                    <i className="bi bi-send" aria-hidden />
                )}
            </button>
        </form>
    );
}
