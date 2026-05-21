import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
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
    // MemoryRouter — WelcomeTiles (rendered inside WelcomeView) uses
    // `Link` from react-router-dom to route to /chat/:id and
    // /automations etc. Without a Router in the tree, `Link` throws
    // on `useContext`.
    return render(
        <MemoryRouter>
            <AuthProvider>
                <WelcomeView {...props} />
            </AuthProvider>
        </MemoryRouter>,
    );
}

// Quick prompts (the spiritual successor of the old "Suggested"
// chips) sit behind the Customize popover. Default active tile is
// `todays-brief`; tests that need the suggestion buttons must open
// the popover and pick `quick-prompts` first.
function switchToQuickPrompts() {
    fireEvent.click(screen.getByTestId("welcome-tiles-customize"));
    fireEvent.click(screen.getByTestId("welcome-tiles-pick-quick-prompts"));
}

describe("WelcomeView", () => {
    it("renders the brand + composer + tile picker", () => {
        renderWelcome({ onSend: () => {} });
        expect(screen.getByTestId("welcome-view")).toBeInTheDocument();
        expect(screen.getByTestId("composer-input")).toBeInTheDocument();
        expect(screen.getByTestId("welcome-tiles")).toBeInTheDocument();
        // Customize button is the affordance for switching tiles.
        expect(screen.getByTestId("welcome-tiles-customize")).toBeInTheDocument();
        // Quick prompts tile renders ≥ 2 prompt buttons once selected.
        switchToQuickPrompts();
        const suggestions = screen.getAllByTestId("welcome-suggestion");
        expect(suggestions.length).toBeGreaterThanOrEqual(2);
    });

    it("clicking a suggestion fires onSend with that prompt text", () => {
        const onSend = vi.fn();
        renderWelcome({ onSend });
        switchToQuickPrompts();
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
