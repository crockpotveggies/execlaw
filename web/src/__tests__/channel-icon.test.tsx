// Per-channel icon resolution chain.
//
// The chain has FOUR precedence layers (see ChannelIcons.tsx
// comment block for full prose):
//   1. Brand-SVG override for channels that bi-* can't render
//      correctly (today: signal).
//   2. Plugin-manifest icon override (`manifestIcon` prop).
//   3. Built-in channel → bi-* mapping for native channels.
//   4. Default fallback `bi-chat-quote`.
//
// These tests pin each branch so a plugin author who adds a new
// transport — and any future host change to the brand-SVG roster
// — surfaces the breakage here rather than in a sidebar pixel diff.

import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { ChannelIcon } from "../components/ChannelIcons";

function classesOf(el: HTMLElement): string[] {
    return Array.from(el.classList);
}

describe("ChannelIcon resolution chain", () => {
    it("renders the Signal brand SVG for channel='signal' (override layer 1)", () => {
        // bi-signal is the cellular-meter glyph — NOT the messenger
        // app — so we inline the Simple Icons SVG path. The element
        // is an <svg>, not an <i class='bi …'>; the test asserts on
        // the tag name to pin which branch ran.
        render(<ChannelIcon channel="signal" decorative />);
        const el = screen.getByTestId("channel-icon");
        expect(el.tagName.toLowerCase()).toBe("svg");
        expect(el).toHaveAttribute("data-channel", "signal");
    });

    it("uses the plugin manifest icon when supplied (layer 2)", () => {
        // Hypothetical "telegram" channel with a manifest-supplied
        // icon = "telegram". The fallback chain in step 3 also has
        // "telegram" mapped, but layer 2 wins regardless — operator
        // can override the host mapping per-plugin.
        render(
            <ChannelIcon
                channel="future-bridge"
                manifestIcon="rocket-takeoff"
                decorative
            />,
        );
        const el = screen.getByTestId("channel-icon");
        expect(el.tagName.toLowerCase()).toBe("i");
        expect(classesOf(el)).toContain("bi-rocket-takeoff");
    });

    it("falls back to KNOWN_CHANNEL_BI when no manifest icon (layer 3)", () => {
        // discord → bi-discord (the actual Bootstrap brand glyph).
        render(<ChannelIcon channel="discord" decorative />);
        const el = screen.getByTestId("channel-icon");
        expect(el.tagName.toLowerCase()).toBe("i");
        expect(classesOf(el)).toContain("bi-discord");
    });

    it("falls back to bi-chat-quote when channel is unknown and no manifest icon (layer 4)", () => {
        // Defends against a future bridge plugin shipping without
        // an icon AND a channel name not in KNOWN_CHANNEL_BI: the
        // sidebar must still render something reasonable, not blank.
        render(<ChannelIcon channel="totally-new-bridge" decorative />);
        const el = screen.getByTestId("channel-icon");
        expect(el.tagName.toLowerCase()).toBe("i");
        expect(classesOf(el)).toContain("bi-chat-quote");
    });

    it("ignores an empty/whitespace-only manifestIcon and falls through", () => {
        // Operator typo or accidental `icon = ""` shouldn't render
        // `bi-` (broken CSS selector). The trim handles both cases.
        render(
            <ChannelIcon channel="whatsapp" manifestIcon="  " decorative />,
        );
        const el = screen.getByTestId("channel-icon");
        expect(classesOf(el)).toContain("bi-whatsapp");
    });

    it("brand override beats manifest icon for signal (layer 1 > layer 2)", () => {
        // Defensive: even if a plugin author sets icon to something
        // weird in the Signal manifest, the brand SVG renders. The
        // brand mark is canonical; a misconfigured icon shouldn't
        // visually break Signal threads.
        render(
            <ChannelIcon
                channel="signal"
                manifestIcon="rocket-takeoff"
                decorative
            />,
        );
        const el = screen.getByTestId("channel-icon");
        expect(el.tagName.toLowerCase()).toBe("svg");
    });

    it("applies monochrome to the Signal brand SVG (currentColor instead of brand blue)", () => {
        render(<ChannelIcon channel="signal" monochrome decorative />);
        const el = screen.getByTestId("channel-icon");
        const path = el.querySelector("path");
        expect(path).not.toBeNull();
        expect(path?.getAttribute("fill")).toBe("currentColor");
    });

    it("stamps the channel name on data-channel for every branch", () => {
        // Stable hook for the sidebar's per-row styling + for the
        // existing message-stream tests that key on data-channel.
        for (const channel of ["signal", "discord", "whatsapp", "unknown"]) {
            const { unmount } = render(
                <ChannelIcon channel={channel} decorative />,
            );
            const el = screen.getByTestId("channel-icon");
            expect(el).toHaveAttribute("data-channel", channel);
            unmount();
        }
    });
});
