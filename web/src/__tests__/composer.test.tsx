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
        expect(onSend).toHaveBeenCalledWith("hello");
    });

    it("Enter submits, Shift+Enter does not", () => {
        const onSend = vi.fn();
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "msg" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
        expect(onSend).not.toHaveBeenCalled();
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("msg");
    });

    it("respects an external `disabled` prop", () => {
        render(<Composer onSend={() => {}} disabled />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        const send = screen.getByTestId("composer-send") as HTMLButtonElement;
        expect(input.disabled).toBe(true);
        expect(send.disabled).toBe(true);
    });
});
