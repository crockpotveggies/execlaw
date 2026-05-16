import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { AuthProvider } from "../auth/AuthContext";
import { WelcomeView } from "../chat/WelcomeView";

// WelcomeView calls `useAuth()` (via `useVoiceReadiness`) so the
// component now needs an AuthProvider in its render tree. The
// readiness hook also fires a `listBackends` request on mount;
// we stub fetch to a 404 so the hook degrades to "voice
// unavailable" without blowing up.
beforeEach(() => {
    const fetchMock = vi.fn(async () => new Response("{}", { status: 404 }));
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

function renderWelcome(props: Parameters<typeof WelcomeView>[0]) {
    return render(
        <AuthProvider>
            <WelcomeView {...props} />
        </AuthProvider>,
    );
}

describe("WelcomeView", () => {
    it("renders the brand + composer + suggestions", () => {
        renderWelcome({ onSend: () => {} });
        expect(screen.getByTestId("welcome-view")).toBeInTheDocument();
        expect(screen.getByTestId("composer-input")).toBeInTheDocument();
        const suggestions = screen.getAllByTestId("welcome-suggestion");
        expect(suggestions.length).toBeGreaterThanOrEqual(2);
    });

    it("clicking a suggestion fires onSend with that prompt text", () => {
        const onSend = vi.fn();
        renderWelcome({ onSend });
        const first = screen.getAllByTestId("welcome-suggestion")[0];
        fireEvent.click(first);
        expect(onSend).toHaveBeenCalledTimes(1);
        // Prompt is non-empty.
        expect(typeof onSend.mock.calls[0][0]).toBe("string");
        expect((onSend.mock.calls[0][0] as string).length).toBeGreaterThan(5);
    });

    it("composer Enter from the welcome view also fires onSend", () => {
        const onSend = vi.fn().mockResolvedValue(undefined);
        renderWelcome({ onSend });
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "hi from welcome" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("hi from welcome", [], []);
    });
});
