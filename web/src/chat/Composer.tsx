// Composer — text input + send button rendered as a single Bootstrap
// input-group so they're visually flush. Auto-grows the textarea up
// to a max height; sends on Enter, Shift+Enter inserts a newline.

import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import InputGroup from "react-bootstrap/InputGroup";
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
