// Connection-banner surface tests.
//
// Covers the rollUp logic + the React surface together — the
// publishers (apiFetch / WsClient) push state, the banner
// component renders the appropriate label.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, render, screen } from "@testing-library/react";
import {
    _resetConnectionState,
    getConnectionStatus,
    reportRestNetworkError,
    reportRestSuccess,
    reportWsState,
} from "../api/connection";
import { ConnectionBanner } from "../routes/ConnectionBanner";

beforeEach(() => {
    _resetConnectionState();
});

afterEach(() => {
    _resetConnectionState();
});

describe("ConnectionBanner", () => {
    it("renders nothing when WS is idle and REST is healthy", () => {
        render(<ConnectionBanner />);
        expect(screen.queryByTestId("connection-banner")).toBeNull();
    });

    it("renders nothing while WS is open", () => {
        render(<ConnectionBanner />);
        act(() => {
            reportWsState("open");
        });
        expect(screen.queryByTestId("connection-banner")).toBeNull();
    });

    it("shows reconnecting (yellow) when WS is reconnecting and REST is fine", () => {
        render(<ConnectionBanner />);
        act(() => {
            reportWsState("reconnecting");
        });
        const banner = screen.getByTestId("connection-banner");
        expect(banner.dataset.status).toBe("reconnecting");
        expect(banner.textContent).toMatch(/Reconnecting/i);
    });

    it("escalates to offline (red) when both WS and REST are failing", () => {
        render(<ConnectionBanner />);
        act(() => {
            reportWsState("reconnecting");
            reportRestNetworkError();
        });
        const banner = screen.getByTestId("connection-banner");
        expect(banner.dataset.status).toBe("offline");
        expect(banner.textContent).toMatch(/Server unreachable/i);
    });

    it("a REST success after a network error clears the offline state", () => {
        render(<ConnectionBanner />);
        act(() => {
            reportWsState("reconnecting");
            reportRestNetworkError();
        });
        expect(screen.getByTestId("connection-banner").dataset.status).toBe(
            "offline",
        );
        act(() => {
            reportRestSuccess();
        });
        // WS still reconnecting, but REST is healthy — drop back to
        // the yellow banner.
        expect(screen.getByTestId("connection-banner").dataset.status).toBe(
            "reconnecting",
        );
    });

    it("REST-alone failure with WS open stays at yellow (transient REST hiccup)", () => {
        render(<ConnectionBanner />);
        act(() => {
            reportWsState("open");
            reportRestNetworkError();
        });
        // REST failing but WS healthy = "reconnecting" (yellow), not
        // "offline" (red). Operator's signal is "the REST round-trip
        // missed once; we'll retry."
        const banner = screen.getByTestId("connection-banner");
        expect(banner.dataset.status).toBe("reconnecting");
    });

    it("getConnectionStatus snapshot matches the rolled-up state", () => {
        expect(getConnectionStatus()).toBe("online");
        reportWsState("reconnecting");
        expect(getConnectionStatus()).toBe("reconnecting");
        reportRestNetworkError();
        expect(getConnectionStatus()).toBe("offline");
    });
});
