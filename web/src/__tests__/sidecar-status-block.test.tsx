// Tests for the shared SidecarStatusBlock — the operator-facing
// "is the sidecar up yet?" card used by Signal + WhatsApp config
// pages. The headline UX behaviour under test: when the sidecar is
// in `pulling` / `starting` / `awaiting_pairing`, an explicit
// "Sidecar is booting up…" header with a spinner appears so the
// operator doesn't see raw status text and assume something is
// wrong.

import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { SidecarStatusBlock } from "../components/SidecarStatusBlock";

const PROPS = {
    sidecarLabel: "signal-cli",
    rpcUrl: null as string | null,
    testidPrefix: "signal",
};

describe("SidecarStatusBlock — booting-state UX", () => {
    it("shows the booting-up header with a spinner during `pulling`", () => {
        render(<SidecarStatusBlock {...PROPS} status="pulling" />);
        const header = screen.getByTestId("signal-sidecar-booting-header");
        expect(header).toBeInTheDocument();
        expect(header.textContent).toMatch(/booting up/i);
        // Spinner has role="status" via react-bootstrap's Spinner.
        const spinner = header.querySelector('[role="status"]');
        expect(spinner).not.toBeNull();
    });

    it("shows the booting-up header during `starting`", () => {
        render(<SidecarStatusBlock {...PROPS} status="starting" />);
        expect(
            screen.getByTestId("signal-sidecar-booting-header"),
        ).toBeInTheDocument();
    });

    it("does NOT show the booting-up header during `awaiting_pairing` (the sidecar IS up at that point — only the next-stage provisioning is pending)", () => {
        render(<SidecarStatusBlock {...PROPS} status="awaiting_pairing" />);
        expect(
            screen.queryByTestId("signal-sidecar-booting-header"),
        ).toBeNull();
    });

    it("does NOT show the booting-up header when healthy", () => {
        render(<SidecarStatusBlock {...PROPS} status="healthy" />);
        expect(
            screen.queryByTestId("signal-sidecar-booting-header"),
        ).toBeNull();
    });

    it("does NOT show the booting-up header when crash-looping", () => {
        render(<SidecarStatusBlock {...PROPS} status="crash_looping" />);
        expect(
            screen.queryByTestId("signal-sidecar-booting-header"),
        ).toBeNull();
    });

    it("does NOT show the booting-up header when stopped", () => {
        render(<SidecarStatusBlock {...PROPS} status="stopped" />);
        expect(
            screen.queryByTestId("signal-sidecar-booting-header"),
        ).toBeNull();
    });
});

describe("SidecarStatusBlock — chip presentation", () => {
    it("renders friendly chip labels rather than raw status strings", () => {
        const cases: Array<[string, RegExp]> = [
            ["pulling", /Pulling image/i],
            ["starting", /^Starting$/],
            ["healthy", /^Healthy$/],
            ["crash_looping", /Crash looping/i],
            ["stopped", /^Stopped$/],
        ];
        for (const [status, expected] of cases) {
            const { unmount } = render(
                <SidecarStatusBlock {...PROPS} status={status} />,
            );
            const chip = screen.getByTestId("signal-sidecar-status");
            expect(chip.textContent).toMatch(expected);
            unmount();
        }
    });

    it("preserves the raw status as a data-status attribute for tests", () => {
        render(<SidecarStatusBlock {...PROPS} status="crash_looping" />);
        const chip = screen.getByTestId("signal-sidecar-status");
        expect(chip.getAttribute("data-status")).toBe("crash_looping");
    });

    it("colors the chip danger-red for crash_looping (the wire format the supervisor actually emits)", () => {
        // Regression guard for the bug the previous local
        // badgeClassForStatus had — it checked `crashlooping`
        // (no underscore) so real `crash_looping` payloads fell
        // through to neutral grey. The shared block now covers
        // both spellings explicitly.
        render(<SidecarStatusBlock {...PROPS} status="crash_looping" />);
        const chip = screen.getByTestId("signal-sidecar-status");
        expect(chip.className).toContain("bg-danger");
    });

    it("falls back to the raw status + neutral chip for unknown values", () => {
        render(<SidecarStatusBlock {...PROPS} status="quantum-superposition" />);
        const chip = screen.getByTestId("signal-sidecar-status");
        expect(chip.textContent).toBe("quantum-superposition");
        expect(chip.className).toContain("bg-secondary");
    });
});

describe("SidecarStatusBlock — operator-actionable copy", () => {
    it("`pulling` explainer mentions Docker image + first-run timing", () => {
        render(<SidecarStatusBlock {...PROPS} status="pulling" />);
        expect(screen.getByText(/Docker image/i)).toBeInTheDocument();
        expect(screen.getByText(/first run/i)).toBeInTheDocument();
    });

    it("`starting` explainer reassures that polling will update on its own", () => {
        render(<SidecarStatusBlock {...PROPS} status="starting" />);
        expect(screen.getByText(/health probe/i)).toBeInTheDocument();
        expect(screen.getByText(/polls every few seconds/i)).toBeInTheDocument();
    });

    it("`crash_looping` explainer points at Settings → Sidecars", () => {
        render(<SidecarStatusBlock {...PROPS} status="crash_looping" />);
        expect(screen.getByText(/Settings → Sidecars/)).toBeInTheDocument();
        expect(screen.getByText(/parked/i)).toBeInTheDocument();
    });

    it("`healthy` shows no explainer paragraph by default", () => {
        const { container } = render(
            <SidecarStatusBlock {...PROPS} status="healthy" />,
        );
        // Only the chip + (no rpcUrl, no fetchError, no followupHint),
        // so the muted explainer div isn't rendered. Just check the
        // booting header isn't there + chip is healthy.
        expect(container.querySelectorAll(".execlaw-muted").length).toBe(0);
    });

    it("`followupHint` overrides the default explainer", () => {
        render(
            <SidecarStatusBlock
                {...PROPS}
                status="awaiting_pairing"
                followupHint={<>auto-creating wuzapi user…</>}
            />,
        );
        expect(
            screen.getByText(/auto-creating wuzapi user/i),
        ).toBeInTheDocument();
    });

    it("`fetchError` surfaces under the chip when present", () => {
        render(
            <SidecarStatusBlock
                {...PROPS}
                status="healthy"
                fetchError="connection refused"
            />,
        );
        expect(
            screen.getByTestId("signal-sidecar-fetch-error"),
        ).toBeInTheDocument();
        expect(
            screen.getByText(/connection refused/),
        ).toBeInTheDocument();
    });

    it("`rpcUrl` is rendered as <code> when supplied", () => {
        render(
            <SidecarStatusBlock
                {...PROPS}
                status="healthy"
                rpcUrl="http://127.0.0.1:8501"
            />,
        );
        expect(screen.getByText("http://127.0.0.1:8501")).toBeInTheDocument();
    });
});

describe("SidecarStatusBlock — testid scoping", () => {
    it("scopes test ids by `testidPrefix` so Signal + WhatsApp can render side-by-side", () => {
        render(
            <SidecarStatusBlock
                {...PROPS}
                status="starting"
                testidPrefix="whatsapp"
                sidecarLabel="wuzapi"
            />,
        );
        expect(
            screen.getByTestId("whatsapp-sidecar-block"),
        ).toBeInTheDocument();
        expect(
            screen.getByTestId("whatsapp-sidecar-status"),
        ).toBeInTheDocument();
        expect(
            screen.getByTestId("whatsapp-sidecar-booting-header"),
        ).toBeInTheDocument();
    });
});
