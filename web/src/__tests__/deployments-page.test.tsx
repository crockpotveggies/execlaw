import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { DeploymentsPage } from "../settings/DeploymentsPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = () =>
    new Response(
        JSON.stringify({
            user_id: "c1",
            username: "u",
            display_name: "U",
            email: null,
            role: "controller",
            last_login_at: null,
        }),
        { status: 200 },
    );

function mountPage() {
    return render(
        <AuthProvider>
            <DeploymentsPage />
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

describe("DeploymentsPage", () => {
    it("renders the empty state when no deployments are configured", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/deployments")
                return new Response(JSON.stringify({ deployments: [] }), {
                    status: 200,
                });
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/No deployments configured/i),
            ).toBeInTheDocument();
        });
        // The "New deployment" button is always available.
        expect(screen.getByTestId("deployments-new")).toBeInTheDocument();
    });

    it("lists each deployment with purpose + backend + default badge", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/deployments")
                return new Response(
                    JSON.stringify({
                        deployments: [
                            {
                                id: "dep-default",
                                purpose: "Standard",
                                inference_backend: "service-vllm",
                                model_spec: { model: "Qwen3.5-27B-AWQ" },
                                gpu_id: null,
                                endpoint: "http://127.0.0.1:8000/v1",
                                is_default: true,
                                active: true,
                                notes: null,
                                created_at: 0,
                                updated_at: 0,
                            },
                            {
                                id: "dep-secondary",
                                purpose: "Standard",
                                inference_backend: "service-other",
                                model_spec: {},
                                gpu_id: null,
                                endpoint: null,
                                is_default: false,
                                active: false,
                                notes: null,
                                created_at: 0,
                                updated_at: 0,
                            },
                        ],
                    }),
                    { status: 200 },
                );
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("deployments-card")).toHaveLength(2);
        });
        // Default + active badges visible on the first row.
        expect(screen.getByText("default")).toBeInTheDocument();
        expect(screen.getByText("inactive")).toBeInTheDocument();
    });

    it("opening the create form shows the JSON model_spec textarea", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            return new Response(JSON.stringify({ deployments: [] }), {
                status: 200,
            });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("deployments-new")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("deployments-new"));
        expect(
            screen.getByTestId("deployments-create-form"),
        ).toBeInTheDocument();
        expect(
            screen.getByTestId("deployments-model-spec"),
        ).toBeInTheDocument();
    });

    it("create form rejects invalid JSON in model_spec without sending", async () => {
        const calls: Array<{ url: string; method: string }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, method: init?.method ?? "GET" });
            if (url === "/api/admin/me") return meResponse();
            return new Response(JSON.stringify({ deployments: [] }), {
                status: 200,
            });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("deployments-new")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("deployments-new"));
        fireEvent.change(screen.getByTestId("deployments-model-spec"), {
            target: { value: "not json" },
        });
        fireEvent.click(screen.getByTestId("deployments-create-submit"));
        await waitFor(() => {
            expect(
                screen.getByText(/model_spec must be valid JSON/i),
            ).toBeInTheDocument();
        });
        // No POST should have fired — we never reached the network.
        expect(
            calls.some(
                (c) =>
                    c.url === "/api/admin/deployments" && c.method === "POST",
            ),
        ).toBe(false);
    });

    it("create form posts the parsed body when JSON is valid", async () => {
        const posted: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            posted.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/deployments" &&
                (init?.method ?? "GET") === "POST"
            ) {
                return new Response(
                    JSON.stringify({
                        id: "dep-new",
                        purpose: "Standard",
                        inference_backend: "service-vllm",
                        model_spec: { model: "Qwen3.5-27B-AWQ" },
                        gpu_id: null,
                        endpoint: "http://127.0.0.1:8000/v1",
                        is_default: true,
                        active: true,
                        notes: null,
                        created_at: 0,
                        updated_at: 0,
                    }),
                    { status: 200 },
                );
            }
            return new Response(JSON.stringify({ deployments: [] }), {
                status: 200,
            });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("deployments-new")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("deployments-new"));
        fireEvent.click(screen.getByTestId("deployments-create-submit"));
        await waitFor(() => {
            expect(
                posted.some(
                    (c) =>
                        c.url === "/api/admin/deployments" &&
                        (c.init?.method ?? "GET") === "POST",
                ),
            ).toBe(true);
        });
        const post = posted.find(
            (c) =>
                c.url === "/api/admin/deployments" &&
                (c.init?.method ?? "GET") === "POST",
        )!;
        const body = JSON.parse((post.init?.body as string) ?? "{}");
        expect(body.purpose).toBe("Standard");
        expect(body.inference_backend).toBe("service-vllm");
        expect(body.model_spec).toEqual({ model: "Qwen3.5-27B-AWQ" });
    });
});
