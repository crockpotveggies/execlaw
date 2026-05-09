// Tests for the standalone /approvals page body.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ApprovalsPage } from "../approvals/ApprovalsPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = () =>
    new Response(
        JSON.stringify({
            user_id: "ctrl-1",
            username: "ctrl",
            display_name: "Ctrl",
            email: null,
            role: "controller",
            last_login_at: null,
        }),
        { status: 200 },
    );

interface ApprovalFixture {
    approval_id: string;
    conversation_id: string;
    sender_principal_id: string;
    original_text: string;
}

function approvalsResponse(approvals: ApprovalFixture[]) {
    return new Response(JSON.stringify({ approvals }), { status: 200 });
}

function mountPage() {
    return render(
        <AuthProvider>
            <ApprovalsPage />
        </AuthProvider>,
    );
}

beforeEach(() => {
    localStorage.setItem("execlaw.access_token", "tok");
    localStorage.setItem("execlaw.refresh_token", "tok");
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("ApprovalsPage", () => {
    it("renders the empty state when there are no pending approvals", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/approvals") return approvalsResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("approvals-empty")).toBeInTheDocument();
        });
        expect(screen.getByTestId("approvals-empty").textContent).toContain(
            "No pending approvals",
        );
    });

    it("renders one card per pending approval with all five verb buttons", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/approvals")
                return approvalsResponse([
                    {
                        approval_id: "appr-1",
                        conversation_id: "conv-1",
                        sender_principal_id: "pri_signal_+15551234567",
                        original_text: "Hey there",
                    },
                    {
                        approval_id: "appr-2",
                        conversation_id: "conv-2",
                        sender_principal_id: "pri_web_anon-9",
                        original_text: "different message",
                    },
                ]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("approval-row")).toHaveLength(2);
        });
        // Each row carries the canonical 5 verbs.
        const verbs = [
            "trust",
            "trust_limited",
            "claim_as_me",
            "ignore_once",
            "block",
        ];
        for (const verb of verbs) {
            expect(
                screen.getAllByTestId(`approval-row-verb-${verb}`),
            ).toHaveLength(2);
        }
        expect(screen.getAllByTestId("approval-row-text")[0].textContent).toContain(
            "Hey there",
        );
    });

    it("POSTs the chosen verb (snake_case) to the respond endpoint", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/approvals" && (!init || init.method !== "POST"))
                return approvalsResponse([
                    {
                        approval_id: "appr-1",
                        conversation_id: "conv-1",
                        sender_principal_id: "pri_signal_+15551234567",
                        original_text: "Hey there",
                    },
                ]);
            if (
                url === "/api/admin/approvals/appr-1/respond" &&
                init?.method === "POST"
            ) {
                return new Response(
                    JSON.stringify({
                        approval_id: "appr-1",
                        principal_id: "controller-x",
                        conversation_id: "conv-1",
                        new_trust_class: "Controller",
                        outcome: "claim_as_me",
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("approval-row")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("approval-row-verb-claim_as_me"));
        await waitFor(() => {
            const respondCall = calls.find(
                (c) =>
                    c.url === "/api/admin/approvals/appr-1/respond" &&
                    c.init?.method === "POST",
            );
            expect(respondCall).toBeDefined();
        });
        const respondCall = calls.find(
            (c) =>
                c.url === "/api/admin/approvals/appr-1/respond" &&
                c.init?.method === "POST",
        )!;
        const body = JSON.parse((respondCall.init?.body as string) ?? "{}");
        // Wire value MUST be snake_case — the server's serde rejects
        // PascalCase. This is the contract the pre-fix code violated.
        expect(body.verb).toBe("claim_as_me");
    });
});
