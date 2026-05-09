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

const FOUR_PURPOSES = ["Standard", "Small", "VoiceSTT", "VoiceTTS"];

function emptyListResponse() {
    return new Response(
        JSON.stringify({
            backends: FOUR_PURPOSES.map((purpose) => ({
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

/// Phase 13.B.1 — minimal preset list the wizard fetches when an
/// "Add backend" click opens it. Tests that target the raw form
/// reuse this then click "I'll type the JSON" to skip the wizard.
function presetsResponseFor(purpose: string) {
    return new Response(
        JSON.stringify({
            purpose,
            detected_vendors: ["nvidia"],
            presets: [
                {
                    id: "vllm-cuda",
                    purpose,
                    inference_backend: "service-vllm",
                    name: "vLLM (NVIDIA)",
                    description: "Fixture",
                    image: "test/image:v1",
                    container_port: 8000,
                    vendor: "nvidia",
                    default_args: [],
                    fields: [
                        {
                            kind: "model_size",
                            label: "Model size",
                            choices: ["small", "medium"],
                            default: "small",
                            arg_template: "--model-size={value}",
                        },
                    ],
                    recommended: true,
                },
                {
                    id: "vllm-cpu",
                    purpose,
                    inference_backend: "service-vllm",
                    name: "vLLM (CPU)",
                    description: "Fixture CPU",
                    image: "test/image-cpu:v1",
                    container_port: 8000,
                    vendor: "cpu",
                    default_args: [],
                    fields: [],
                    recommended: false,
                },
            ],
        }),
        { status: 200 },
    );
}

/// Helper that skips the wizard the way an operator would when
/// they want to type the JSON directly. Many existing tests target
/// the raw form, so they call this right after clicking "Add backend".
///
/// Phase 14 follow-up: the inline wizard is now `UnifiedBackendForm`
/// with per-purpose test IDs (`backends-wizard-{Purpose}-...`). This
/// helper assumes the operator just clicked the Standard row's edit
/// button — that's the alphabetically-first row and the path every
/// existing caller takes.
async function skipWizard(purpose: string = "Standard") {
    const formTid = `backends-wizard-${purpose}-form`;
    const skipTid = `backends-wizard-${purpose}-skip`;
    await waitFor(() => {
        expect(screen.getByTestId(formTid)).toBeInTheDocument();
    });
    fireEvent.click(screen.getByTestId(skipTid));
    await waitFor(() => {
        expect(screen.getByTestId("backend-form")).toBeInTheDocument();
    });
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
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
        });
        for (const p of FOUR_PURPOSES) {
            expect(screen.getByText(p)).toBeInTheDocument();
        }
        // All four start as "not configured".
        expect(screen.getAllByText(/not configured/i)).toHaveLength(4);
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
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
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
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            // The legacy preset endpoint is no longer fetched by the
            // BackendsPage but some tests mock it for backwards-compat.
            if (url.startsWith("/api/admin/backends/presets"))
                return presetsResponseFor("Standard");
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return emptyListResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
        });
        // Click the Standard row's "Add backend" button (first edit btn).
        const editButtons = screen.getAllByTestId("backend-edit");
        // Standard is alphabetically first in the BACKEND_PURPOSES array
        // and the Settings page renders that order, so editButtons[0] is
        // the Standard slot.
        fireEvent.click(editButtons[0]);
        // 13.B.1 — fresh adds open the wizard first; this test
        // targets the raw form, so skip into it.
        await skipWizard();
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
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            // The legacy preset endpoint is no longer fetched by the
            // BackendsPage but some tests mock it for backwards-compat.
            if (url.startsWith("/api/admin/backends/presets"))
                return presetsResponseFor("Standard");
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return emptyListResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
        });
        const editButtons = screen.getAllByTestId("backend-edit");
        fireEvent.click(editButtons[0]);
        await skipWizard();
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
                        backends: FOUR_PURPOSES.map((purpose) => ({
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

    // ---- Phase 12.D — Mode toggle + status pill -----------------------

    function listResponseWithManagedStandard() {
        return new Response(
            JSON.stringify({
                backends: FOUR_PURPOSES.map((purpose) => ({
                    purpose,
                    configured: purpose === "Standard",
                    backend:
                        purpose === "Standard"
                            ? {
                                  purpose,
                                  inference_backend: "service-vllm",
                                  model_spec: { image: "vllm:test" },
                                  gpu_id: "0",
                                  endpoint: "http://127.0.0.1:8101",
                                  notes: null,
                                  reasoning_enabled: false,
                                  supports_reasoning_toggle: true,
                                  mode: "managed",
                                  created_at: 0,
                                  updated_at: 0,
                              }
                            : null,
                })),
            }),
            { status: 200 },
        );
    }

    it("renders managed badge + Healthy status pill on managed rows", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends")
                return listResponseWithManagedStandard();
            if (url === "/api/admin/backends/Standard/status") {
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        mode: "managed",
                        status: "Healthy",
                        endpoint: "http://127.0.0.1:8101",
                        restart_attempts: 0,
                        supervisor_available: true,
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("backend-mode-badge")).toBeInTheDocument();
        });
        await waitFor(() => {
            expect(
                screen.getByTestId("backend-status-pill"),
            ).toHaveTextContent("Healthy");
        });
    });

    it("renders 'Docker offline' when supervisor_available is false", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends")
                return listResponseWithManagedStandard();
            if (url === "/api/admin/backends/Standard/status") {
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        mode: "managed",
                        status: "Stopped",
                        endpoint: null,
                        restart_attempts: 0,
                        supervisor_available: false,
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("backend-status-pill"),
            ).toHaveTextContent("Docker offline");
        });
    });

    it("PUTs mode=managed and null endpoint when toggle picks managed", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends" && (!init || init.method === "GET" || init.method === undefined))
                return emptyListResponse();
            if (
                url === "/api/admin/backends/Standard" &&
                init?.method === "PUT"
            ) {
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        inference_backend: "service-vllm",
                        model_spec: { image: "vllm:test" },
                        gpu_id: "0",
                        endpoint: null,
                        notes: null,
                        reasoning_enabled: false,
                        supports_reasoning_toggle: true,
                        mode: "managed",
                        created_at: 0,
                        updated_at: 0,
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            // The legacy preset endpoint is no longer fetched by the
            // BackendsPage but some tests mock it for backwards-compat.
            if (url.startsWith("/api/admin/backends/presets"))
                return presetsResponseFor("Standard");
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
        });
        const editButtons = screen.getAllByTestId("backend-edit");
        // Standard is alphabetically first.
        fireEvent.click(editButtons[0]);
        await skipWizard();
        fireEvent.click(screen.getByTestId("backend-form-mode-managed"));
        fireEvent.change(screen.getByTestId("backend-form-backend"), {
            target: { value: "service-vllm" },
        });
        fireEvent.change(screen.getByTestId("backend-form-model-spec"), {
            target: {
                value: '{"image":"vllm:test","args":["--model","X"],"container_port":8000}',
            },
        });
        // Endpoint field is disabled in managed mode — verify and skip
        // typing.
        const endpointField = screen.getByTestId(
            "backend-form-endpoint",
        ) as HTMLInputElement;
        expect(endpointField.disabled).toBe(true);
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
        expect(body.mode).toBe("managed");
        expect(body.endpoint).toBeNull();
    });

    // ---- Phase 14 follow-up — managed-backend UnifiedBackendForm ------

    it("opens the wizard inline when adding a fresh backend", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return emptyListResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
        });
        fireEvent.click(screen.getAllByTestId("backend-edit")[0]);
        // Wizard, not the raw form.
        await waitFor(() => {
            expect(
                screen.getByTestId("backends-wizard-Standard-form"),
            ).toBeInTheDocument();
        });
        expect(screen.queryByTestId("backend-form")).toBeNull();
    });

    it("'I'll type the JSON' skip button shows the raw form", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            return emptyListResponse();
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getAllByTestId("backend-row")).toHaveLength(4);
        });
        fireEvent.click(screen.getAllByTestId("backend-edit")[0]);
        await waitFor(() => {
            expect(
                screen.getByTestId("backends-wizard-Standard-form"),
            ).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("backends-wizard-Standard-skip"));
        await waitFor(() => {
            expect(screen.getByTestId("backend-form")).toBeInTheDocument();
        });
        // Wizard is gone.
        expect(
            screen.queryByTestId("backends-wizard-Standard-form"),
        ).toBeNull();
    });

    it("editing an already-configured row goes straight to the raw form", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/backends")
                return listResponseWithManagedStandard();
            if (url === "/api/admin/backends/Standard/status")
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        mode: "managed",
                        status: "Healthy",
                        endpoint: "http://127.0.0.1:8101",
                        restart_attempts: 0,
                        supervisor_available: true,
                    }),
                    { status: 200 },
                );
            if (url === "/api/admin/hardware") return hardwareNoGpu();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            // The legacy preset endpoint is no longer fetched by the
            // BackendsPage but some tests mock it for backwards-compat.
            if (url.startsWith("/api/admin/backends/presets"))
                return presetsResponseFor("Standard");
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("backend-mode-badge")).toBeInTheDocument();
        });
        // Click Edit on the configured Standard row.
        fireEvent.click(screen.getAllByTestId("backend-edit")[0]);
        // Goes straight to form, no wizard.
        await waitFor(() => {
            expect(screen.getByTestId("backend-form")).toBeInTheDocument();
        });
        expect(
            screen.queryByTestId("backends-wizard-Standard-form"),
        ).toBeNull();
        // "Re-run wizard" button is exposed for managed rows.
        expect(
            screen.getByTestId("backend-form-rerun-wizard"),
        ).toBeInTheDocument();
    });
});
