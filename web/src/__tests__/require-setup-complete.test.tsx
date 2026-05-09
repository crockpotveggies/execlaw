// Phase 14 — guard tests for the wizard-incomplete redirect.
//
// AppBoot routes /api/ping at app load. Direct deep links to /chat
// or /settings/* skip AppBoot and need the RequireSetupComplete
// guard to re-probe ping and bounce the operator to /setup if the
// wizard hasn't been completed.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { RequireSetupComplete } from "../routes/RequireSetupComplete";

let fetchMock: ReturnType<typeof vi.fn>;

function mountAt(path: string) {
    return render(
        <MemoryRouter initialEntries={[path]}>
            <Routes>
                <Route
                    path="/chat"
                    element={
                        <RequireSetupComplete>
                            <div data-testid="chat-shell">chat</div>
                        </RequireSetupComplete>
                    }
                />
                <Route path="/setup" element={<div data-testid="wizard">wizard</div>} />
            </Routes>
        </MemoryRouter>,
    );
}

beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("RequireSetupComplete", () => {
    it("ping=pong → renders the wrapped page", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/ping") return new Response("pong", { status: 200 });
            return new Response("{}", { status: 200 });
        });
        mountAt("/chat");
        await waitFor(() => {
            expect(screen.getByTestId("chat-shell")).toBeInTheDocument();
        });
    });

    it("ping=wizard → bounces to /setup", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/ping") return new Response("wizard", { status: 200 });
            return new Response("{}", { status: 200 });
        });
        mountAt("/chat");
        await waitFor(() => {
            expect(screen.getByTestId("wizard")).toBeInTheDocument();
        });
        expect(screen.queryByTestId("chat-shell")).toBeNull();
    });

    it("ping=setup → bounces to /setup (account-creation flow)", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/ping") return new Response("setup", { status: 200 });
            return new Response("{}", { status: 200 });
        });
        mountAt("/chat");
        await waitFor(() => {
            expect(screen.getByTestId("wizard")).toBeInTheDocument();
        });
    });

    it("ping fails → falls through to wrapped page (transient blip ≠ trap)", async () => {
        fetchMock.mockImplementation(async () => {
            throw new Error("network down");
        });
        mountAt("/chat");
        // The guard treats errors as "let the inner page handle the
        // user-visible failure" rather than redirecting to /setup,
        // so the chat shell renders and its own connection-error
        // banner takes over.
        await waitFor(() => {
            expect(screen.getByTestId("chat-shell")).toBeInTheDocument();
        });
    });
});
