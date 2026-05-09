// Phase 13.C audit closure — VoiceStatusBar renders the transcript
// banner + the documented empty-final fallback, which is the SPA's
// only signal for the Opus → PCM codec gap (docs/voice-followups.md).

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { VoiceStatusBar } from "../chat/VoiceStatusBar";

describe("VoiceStatusBar", () => {
    it("renders nothing when transcript is null", () => {
        const { container } = render(
            <VoiceStatusBar transcript={null} sendVoiceControl={() => true} />,
        );
        expect(container.firstChild).toBeNull();
    });

    it("renders 'Listening…' for a non-final transcript", () => {
        render(
            <VoiceStatusBar
                transcript={{
                    session: "s1",
                    text: "hel",
                    is_final: false,
                }}
                sendVoiceControl={() => true}
            />,
        );
        const bar = screen.getByTestId("voice-status-bar");
        expect(bar).toHaveTextContent(/Listening/);
        expect(bar).toHaveTextContent("hel");
        // No Interrupt button while still listening.
        expect(screen.queryByTestId("voice-interrupt")).toBeNull();
    });

    it("renders the final transcript with an Interrupt button", () => {
        render(
            <VoiceStatusBar
                transcript={{
                    session: "s1",
                    text: "hello world",
                    is_final: true,
                }}
                sendVoiceControl={() => true}
            />,
        );
        const bar = screen.getByTestId("voice-status-bar");
        expect(bar).toHaveTextContent("hello world");
        expect(bar.getAttribute("data-empty-final")).toBe("false");
        expect(screen.getByTestId("voice-interrupt")).toBeInTheDocument();
    });

    it("renders the empty-final fallback when text is empty", () => {
        // The audit's UX-trap fix: server returns is_final=true with
        // empty text (silence, or the documented Opus→PCM codec gap).
        // The bar must render an explicit message instead of a blank.
        render(
            <VoiceStatusBar
                transcript={{
                    session: "s1",
                    text: "",
                    is_final: true,
                }}
                sendVoiceControl={() => true}
            />,
        );
        const bar = screen.getByTestId("voice-status-bar");
        expect(bar.getAttribute("data-empty-final")).toBe("true");
        expect(bar).toHaveTextContent(/didn't return a transcript/i);
        // Interrupt button hidden — there's nothing to interrupt.
        expect(screen.queryByTestId("voice-interrupt")).toBeNull();
    });

    it("treats whitespace-only finals as empty", () => {
        render(
            <VoiceStatusBar
                transcript={{
                    session: "s1",
                    text: "   \t  ",
                    is_final: true,
                }}
                sendVoiceControl={() => true}
            />,
        );
        const bar = screen.getByTestId("voice-status-bar");
        expect(bar.getAttribute("data-empty-final")).toBe("true");
    });

    it("Interrupt button fires sendVoiceControl with voice_interrupt + session id", () => {
        const sent: object[] = [];
        render(
            <VoiceStatusBar
                transcript={{
                    session: "abc-123",
                    text: "stop me",
                    is_final: true,
                }}
                sendVoiceControl={(payload) => {
                    sent.push(payload);
                    return true;
                }}
            />,
        );
        fireEvent.click(screen.getByTestId("voice-interrupt"));
        expect(sent).toEqual([
            { op: "voice_interrupt", session: "abc-123" },
        ]);
    });

    it("non-final empty transcript still shows Listening… (not the empty fallback)", () => {
        // The empty-final fallback should NOT fire while the user is
        // still mid-utterance (is_final=false). That would be a UX
        // regression — the bar should say "Listening…" until the
        // server flushes a real final.
        render(
            <VoiceStatusBar
                transcript={{
                    session: "s1",
                    text: "",
                    is_final: false,
                }}
                sendVoiceControl={vi.fn()}
            />,
        );
        const bar = screen.getByTestId("voice-status-bar");
        expect(bar.getAttribute("data-empty-final")).toBe("false");
        expect(bar).toHaveTextContent(/Listening/);
    });
});
