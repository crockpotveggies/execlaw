import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { WelcomeView } from "../chat/WelcomeView";

describe("WelcomeView", () => {
    it("renders the brand + composer + suggestions", () => {
        render(<WelcomeView onSend={() => {}} />);
        expect(screen.getByTestId("welcome-view")).toBeInTheDocument();
        expect(screen.getByTestId("composer-input")).toBeInTheDocument();
        const suggestions = screen.getAllByTestId("welcome-suggestion");
        expect(suggestions.length).toBeGreaterThanOrEqual(2);
    });

    it("clicking a suggestion fires onSend with that prompt text", () => {
        const onSend = vi.fn();
        render(<WelcomeView onSend={onSend} />);
        const first = screen.getAllByTestId("welcome-suggestion")[0];
        fireEvent.click(first);
        expect(onSend).toHaveBeenCalledTimes(1);
        // Prompt is non-empty.
        expect(typeof onSend.mock.calls[0][0]).toBe("string");
        expect((onSend.mock.calls[0][0] as string).length).toBeGreaterThan(5);
    });

    it("composer Enter from the welcome view also fires onSend", () => {
        const onSend = vi.fn().mockResolvedValue(undefined);
        render(<WelcomeView onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "hi from welcome" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("hi from welcome");
    });
});
