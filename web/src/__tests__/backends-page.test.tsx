// Tests for the Settings → Backends page (Phase 8.5).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { BackendsPage } from "../settings/BackendsPage";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = (role: "controller" | "operator" = "controller") =>
    new Response(
        JSON.stringify({
            user_id: "ctrl-1",
            username: "ctrl",
            display_name: "Ctrl",
            email: null,
            role,
            last_login_at: null,
        }),
        { status: 200 },
    );

const fivePurposes = ["Standard", "Reasoning", "Guardrail", "VoiceSTT", "VoiceTTS"];

function emptyListResponse() {
    return new Response(
        JSON.stringify({
            backends: fivePurposes.map((purpose) => ({
                purpose,
                configured: false,
                backend: null,
            })),
        }),
        { status: 200 },
    );
}

// Response bodies can only be read once, so build a fresh response
// per call instead of caching a singleton.
function hardwareNoGpu() {
    return new Response(JSON.stringify({ gpus: [] }), { status: 200 });
}

function hardwareWithGpu() {
    return new Response(
        JSON.stringify({
            gpus: [
                {
                    vendor: "NVIDIA",
                    model: "RTX 4090",
                    pci_vendor_id: "10de",
                    pci_device_id: "2684",
                },
            ],
        }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <BackendsPage />
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

describe("BackendsPage", () => {
    it("renders one row per fixed purpose even when nothing is configured", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends") return emptyListResponse();
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(5);
        });
        for (const p of fivePurposes) {
            expect(screen.getByText(p)).toBeInTheDocument();
        }
        // All five start as "not configured".
        expect(screen.getAllByText(/not configured/i)).toHaveLength(5);
    });

    it("does NOT render a + New affordance — purposes are fixed", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends") return emptyListResponse();
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(5);
        });
        // No "+ New" or "Add backend" outside of a row's edit affordance.
        // We assert there's no global +New button by inspecting the page-
        // header region: the only buttons up there are Refresh.
        const headerButtons = screen.getAllByRole("button");
        const labels = headerButtons.map((b) => b.textContent ?? "");
        expect(labels.filter((l) => l.includes("New"))).toHaveLength(0);
    });

    it("PUTs the right body when saving the Standard purpose", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/backends/Standard" &&
                init?.method === "PUT"
            ) {
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        inference_backend: "service-vllm",
                        model_spec: { model: "Qwen3.5-27B-AWQ" },
                        gpu_id: "0",
                        endpoint: "http://127.0.0.1:8000/v1",
                        notes: null,
                        created_at: 0,
                        updated_at: 0,
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return emptyListResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(5);
        });
        // Click the Standard row's "Add backend" button (first edit btn).
        const editButtons = screen.getAllByTestId("backend-edit");
        // Standard is alphabetically first in the BACKEND_PURPOSES array
        // and the Settings page renders that order, so editButtons[0] is
        // the Standard slot.
        fireEvent.click(editButtons[0]);
        await waitFor(() => {
            expect(screen.getByTestId("backend-form")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("backend-form-backend"), {
            target: { value: "service-vllm" },
        });
        fireEvent.change(screen.getByTestId("backend-form-model-spec"), {
            target: { value: '{"model":"Qwen3.5-27B-AWQ"}' },
        });
        fireEvent.change(screen.getByTestId("backend-form-gpu"), {
            target: { value: "0" },
        });
        fireEvent.change(screen.getByTestId("backend-form-endpoint"), {
            target: { value: "http://127.0.0.1:8000/v1" },
        });
        fireEvent.click(screen.getByTestId("backend-form-save"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/backends/Standard" &&
                        c.init?.method === "PUT",
                ),
            ).toBe(true);
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/backends/Standard" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.inference_backend).toBe("service-vllm");
        expect(body.model_spec).toEqual({ model: "Qwen3.5-27B-AWQ" });
        expect(body.gpu_id).toBe("0");
        expect(body.endpoint).toBe("http://127.0.0.1:8000/v1");
    });

    it("rejects invalid JSON in model_spec without sending", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return emptyListResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(5);
        });
        const editButtons = screen.getAllByTestId("backend-edit");
        fireEvent.click(editButtons[0]);
        fireEvent.change(screen.getByTestId("backend-form-backend"), {
            target: { value: "service-vllm" },
        });
        fireEvent.change(screen.getByTestId("backend-form-model-spec"), {
            target: { value: "not json" },
        });
        fireEvent.click(screen.getByTestId("backend-form-save"));
        await waitFor(() => {
            expect(
                screen.getByText(/model_spec must be valid JSON/i),
            ).toBeInTheDocument();
        });
        expect(
            calls.some(
                (c) =>
                    c.url.startsWith("/api/admin/backends/") &&
                    c.init?.method === "PUT",
            ),
        ).toBe(false);
    });

    it("operators see read-only — no Edit / Clear", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/backends")
                return new Response(
                    JSON.stringify({
                        backends: fivePurposes.map((purpose) => ({
                            purpose,
                            configured: purpose === "Standard",
                            backend:
                                purpose === "Standard"
                                    ? {
                                          purpose,
                                          inference_backend: "service-vllm",
                                          model_spec: {},
                                          gpu_id: null,
                                          endpoint: null,
                                          notes: null,
                                          created_at: 0,
                                          updated_at: 0,
                                      }
                                    : null,
                        })),
                    }),
                    { status: 200 },
                );
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(
                    /Only Controllers can change backend configuration/i,
                ),
            ).toBeInTheDocument();
        });
        expect(screen.queryByTestId("backend-edit")).toBeNull();
        expect(screen.queryByTestId("backend-clear")).toBeNull();
    });

    it("renders the hardware section below the backend list", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends") return emptyListResponse();
            if (url === "/api/admin/hardware") return hardwareWithGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("settings-hardware")).toBeInTheDocument();
        });
        expect(screen.getByText(/GPUs \(1\)/i)).toBeInTheDocument();
        expect(screen.getAllByText(/RTX 4090/).length).toBeGreaterThan(0);
    });

    it("hardware section reports no GPUs when the profile is empty", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends") return emptyListResponse();
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByText(/no gpus detected/i),
            ).toBeInTheDocument();
        });
    });
});
