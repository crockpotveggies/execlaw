import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Composer } from "../chat/Composer";

describe("Composer", () => {
    it("send button is disabled when input is empty", () => {
        render(<Composer onSend={() => {}} />);
        const send = screen.getByTestId("composer-send") as HTMLButtonElement;
        expect(send.disabled).toBe(true);
    });

    it("calls onSend with the trimmed text on submit", async () => {
        const onSend = vi.fn().mockResolvedValue(undefined);
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "  hello  " } });
        fireEvent.submit(input.closest("form")!);
        expect(onSend).toHaveBeenCalledWith("hello", []);
    });

    it("Enter submits, Shift+Enter does not", () => {
        const onSend = vi.fn();
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "msg" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
        expect(onSend).not.toHaveBeenCalled();
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("msg", []);
    });

    it("respects an external `disabled` prop", () => {
        render(<Composer onSend={() => {}} disabled />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        const send = screen.getByTestId("composer-send") as HTMLButtonElement;
        expect(input.disabled).toBe(true);
        expect(send.disabled).toBe(true);
    });

    /// 2026-04-28 regression: hitting Enter used to disable the
    /// textarea via `submitting=true`, which blurred the element
    /// (disabled inputs lose focus). The user lost their place
    /// every time they sent a message. The fix only disables on
    /// the explicit `disabled` prop; the submit guard inside
    /// `submit()` still prevents double-sends.
    it("textarea stays focused (and editable) across an Enter-driven submit", async () => {
        // Long-running onSend simulates the agent streaming a reply
        // — the textarea must NOT lock the user out during that
        // window.
        type Resolver = () => void;
        const resolverHolder: { fn: Resolver | null } = { fn: null };
        const onSend = vi.fn(
            (): Promise<void> =>
                new Promise<void>((resolve) => {
                    resolverHolder.fn = resolve as Resolver;
                }),
        );
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        input.focus();
        expect(document.activeElement).toBe(input);

        fireEvent.change(input, { target: { value: "first" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("first", []);

        // Mid-await: textarea should still be focused AND editable
        // so the operator can compose the next thought while the
        // agent streams.
        expect(document.activeElement).toBe(input);
        expect(input.disabled).toBe(false);
        fireEvent.change(input, { target: { value: "second draft" } });
        expect(input.value).toBe("second draft");

        // Resolve the in-flight send — composer flips out of
        // submitting; nothing else should change because the
        // textarea was never disabled in the first place.
        resolverHolder.fn?.();
        await Promise.resolve();
        expect(document.activeElement).toBe(input);
        expect(input.disabled).toBe(false);
    });

    /// Hitting Enter twice in quick succession should NOT fire
    /// onSend twice — the in-flight submit guard catches the
    /// second one. Belt-and-suspenders for the no-disable change
    /// above; without the guard, hot-typing would queue duplicate
    /// turns server-side.
    it("guards against double-submits while a send is in flight", () => {
        type Resolver = () => void;
        const resolverHolder: { fn: Resolver | null } = { fn: null };
        const onSend = vi.fn(
            (): Promise<void> =>
                new Promise<void>((resolve) => {
                    resolverHolder.fn = resolve as Resolver;
                }),
        );
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "first" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        // Without resolving onSend, hammer Enter again. The guard
        // should suppress the second submit.
        fireEvent.change(input, { target: { value: "second" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledTimes(1);
        resolverHolder.fn?.();
    });
});
