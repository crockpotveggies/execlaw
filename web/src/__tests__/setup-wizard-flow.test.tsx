// Phase 14 — first-run setup wizard end-to-end SPA flow.
//
// Covers the three-step stepper: account → docker → backend.
//
// `setup-wizard.test.tsx` already covers `validateSetupForm`
// pure-input validation; this file mounts the full component +
// AuthProvider and asserts the multi-step navigation, conditional
// rendering, and the eventual `PUT /api/admin/backends/Standard`
// payload for both managed and external paths.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Routes, Route } from "react-router-dom";
import { SetupWizard } from "../routes/SetupWizard";
import { AuthProvider } from "../auth/AuthContext";

let fetchMock: ReturnType<typeof vi.fn>;

interface FetchedCall {
    url: string;
    init?: RequestInit;
}

function recordedFetch(): FetchedCall[] {
    return calls;
}

let calls: FetchedCall[] = [];

function mountWizard() {
    return render(
        <MemoryRouter initialEntries={["/setup"]}>
            <AuthProvider>
                <Routes>
                    <Route path="/setup" element={<SetupWizard />} />
                    <Route path="/chat" element={<div data-testid="chat-shell">chat</div>} />
                </Routes>
            </AuthProvider>
        </MemoryRouter>,
    );
}

const setupResponse = () =>
    new Response(
        JSON.stringify({
            principal_id: "ctrl-1",
            access_token: "access-tok",
            refresh_token: "refresh-tok",
        }),
        { status: 200 },
    );

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

const presetsResponseFor = (purpose: string) =>
    new Response(
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
                    image: "vllm/vllm-openai:v0.6.2",
                    container_port: 8000,
                    vendor: "nvidia",
                    default_args: ["--gpu-memory-utilization=0.9"],
                    fields: [
                        {
                            kind: "model",
                            label: "Model",
                            choices: ["QuantTrio/Qwen3.5-27B-AWQ"],
                            default: "QuantTrio/Qwen3.5-27B-AWQ",
                            arg_template: "--model={value}",
                        },
                    ],
                    recommended: true,
                },
            ],
        }),
        { status: 200 },
    );

const upsertBackendResponse = () =>
    new Response(
        JSON.stringify({
            purpose: "Standard",
            inference_backend: "service-vllm",
            model_spec: {},
            gpu_id: "0x10de:0x2684",
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

beforeEach(() => {
    calls = [];
    localStorage.clear();
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

async function fillAndSubmitAccount() {
    fireEvent.change(screen.getByLabelText(/^Username/i), {
        target: { value: "ctrl" },
    });
    fireEvent.change(screen.getByLabelText(/Display name/i), {
        target: { value: "Ctrl" },
    });
    fireEvent.change(screen.getByLabelText(/Admin password/i), {
        target: { value: "hunter2-longer" },
    });
    fireEvent.click(screen.getByTestId("setup-account-submit"));
}

describe("SetupWizard — multi-step flow", () => {
    it("step 1 → step 2: after account creation, surfaces the docker check", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
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
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        expect(screen.getByText(/Docker is reachable/i)).toBeInTheDocument();
        expect(screen.getByText(/24\.0\.7/)).toBeInTheDocument();
    });

    it("docker missing → renders install link + retry button", async () => {
        let preflightCallCount = 0;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                preflightCallCount += 1;
                return new Response(
                    JSON.stringify({
                        docker: { available: false, version: null },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-missing")).toBeInTheDocument();
        });
        const link = screen.getByTestId("setup-docker-install-link");
        expect(link).toHaveAttribute("href", expect.stringMatching(/docker-desktop/));
        expect(link).toHaveAttribute("target", "_blank");

        // Retry probes preflight again.
        const before = preflightCallCount;
        fireEvent.click(screen.getByTestId("setup-docker-retry"));
        await waitFor(() => {
            expect(preflightCallCount).toBeGreaterThan(before);
        });
    });

    it("docker missing → Skip advances to backend step (external form)", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: false, version: null },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-missing")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-skip"));
        // No Docker = external backend form.
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-external")).toBeInTheDocument();
        });
        expect(
            screen.getByText(/Docker isn't reachable/i),
        ).toBeInTheDocument();
    });

    it("docker reachable + no GPU → external backend form with no-GPU rationale", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
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
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-continue"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-external")).toBeInTheDocument();
        });
        // The rationale string includes the parenthetical vendor
        // list — that's the discriminator from the hardware-summary
        // card, which also says "No supported GPU detected".
        expect(
            screen.getByText(/NVIDIA \/ Intel Arc \/ AMD/i),
        ).toBeInTheDocument();
    });

    it("external form: validates URL + saves with mode=external", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
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
            if (
                url === "/api/admin/backends/Standard" &&
                init?.method === "PUT"
            ) {
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        inference_backend: "external",
                        model_spec: { model: "Qwen3.5-27B-AWQ" },
                        gpu_id: null,
                        endpoint: "http://localhost:8000/v1",
                        notes: null,
                        reasoning_enabled: false,
                        supports_reasoning_toggle: true,
                        mode: "external",
                        created_at: 0,
                        updated_at: 0,
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-continue"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-external")).toBeInTheDocument();
        });

        // Empty submit → validation error.
        fireEvent.click(screen.getByTestId("setup-external-submit"));
        await waitFor(() => {
            expect(screen.getByText(/Required/i)).toBeInTheDocument();
        });

        // Garbage URL → validation error.
        fireEvent.change(screen.getByTestId("setup-external-endpoint"), {
            target: { value: "not a url" },
        });
        fireEvent.click(screen.getByTestId("setup-external-submit"));
        await waitFor(() => {
            expect(screen.getByText(/Doesn't look like a URL/i)).toBeInTheDocument();
        });

        // Real URL + model → success.
        fireEvent.change(screen.getByTestId("setup-external-endpoint"), {
            target: { value: "http://localhost:8000/v1" },
        });
        fireEvent.change(screen.getByTestId("setup-external-model"), {
            target: { value: "Qwen3.5-27B-AWQ" },
        });
        fireEvent.click(screen.getByTestId("setup-external-submit"));

        await waitFor(() => {
            expect(screen.getByTestId("chat-shell")).toBeInTheDocument();
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/backends/Standard" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.mode).toBe("external");
        expect(body.endpoint).toBe("http://localhost:8000/v1");
        expect(body.model_spec).toEqual({ model: "Qwen3.5-27B-AWQ" });
    });

    it("docker + GPUs → renders managed BackendWizardPanel", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [
                            {
                                id: "0x10de:0x2684",
                                vendor: "Nvidia",
                                pci_vendor_id: "0x10de",
                                pci_device_id: "0x2684",
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            if (url.startsWith("/api/admin/backends/presets")) {
                return presetsResponseFor("Standard");
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-continue"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-managed")).toBeInTheDocument();
        });
        // No multi-GPU picker for a single GPU host.
        expect(screen.queryByTestId("setup-gpu-picker")).toBeNull();
        // BackendWizardPanel renders.
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
    });

    it("multi-GPU: picker is visible + chosen id is sent in PUT", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [
                            {
                                id: "0x10de:0x2684",
                                vendor: "Nvidia",
                                pci_vendor_id: "0x10de",
                                pci_device_id: "0x2684",
                            },
                            {
                                id: "0x8086:0xe20b",
                                vendor: "Intel",
                                pci_vendor_id: "0x8086",
                                pci_device_id: "0xe20b",
                            },
                        ],
                    }),
                    { status: 200 },
                );
            }
            if (url.startsWith("/api/admin/backends/presets")) {
                return presetsResponseFor("Standard");
            }
            if (
                url === "/api/admin/backends/Standard" &&
                init?.method === "PUT"
            ) {
                return upsertBackendResponse();
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-continue"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-gpu-picker")).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "setup-gpu-picker-select",
        ) as HTMLSelectElement;
        // Two options surfaced.
        expect(select.options.length).toBe(2);
        // Pick the second GPU (Intel Arc).
        fireEvent.change(select, { target: { value: "1" } });
        // Click "Use this preset" inside the embedded BackendWizardPanel.
        await waitFor(() => {
            expect(
                screen.getByTestId("backend-wizard-apply"),
            ).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("backend-wizard-apply"));
        await waitFor(() => {
            expect(screen.getByTestId("chat-shell")).toBeInTheDocument();
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/backends/Standard" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.mode).toBe("managed");
        // gpu_id should match the second GPU's id (Intel Arc).
        expect(body.gpu_id).toBe("0x8086:0xe20b");
    });

    // ---- Timeline indicator (Microsoft 365-style) -----------------------

    it("step indicator: account is current, others upcoming on first mount", async () => {
        fetchMock.mockImplementation(async () => new Response("{}", { status: 200 }));
        mountWizard();
        await waitFor(() => {
            expect(screen.getByTestId("setup-step-indicator")).toBeInTheDocument();
        });
        expect(
            screen.getByTestId("setup-step-account").getAttribute("data-status"),
        ).toBe("current");
        expect(
            screen.getByTestId("setup-step-docker").getAttribute("data-status"),
        ).toBe("upcoming");
        expect(
            screen.getByTestId("setup-step-backend").getAttribute("data-status"),
        ).toBe("upcoming");
    });

    it("step indicator: account flips to done after submission, docker becomes current", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        // Pretend Docker is missing so the docker step
                        // stays "current" rather than auto-flipping
                        // to "done" via the special-case in the
                        // indicator.
                        docker: { available: false, version: null },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-step-account").getAttribute("data-status"),
            ).toBe("done");
        });
        expect(
            screen.getByTestId("setup-step-docker").getAttribute("data-status"),
        ).toBe("current");
    });

    it("step indicator: docker flips to done in-place when preflight succeeds", async () => {
        // Mirrors the Microsoft-365 example — once the prerequisite
        // is met, the step shows the green check immediately rather
        // than waiting for the operator to click Continue.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
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
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        // While still on the docker step, the indicator should
        // already mark it done because preflight came back OK.
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-step-docker").getAttribute("data-status"),
            ).toBe("done");
        });
    });

    // ---- Backend step refresh button ------------------------------------

    it("backend step: hardware refresh button re-runs preflight", async () => {
        let preflightCallCount = 0;
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                preflightCallCount += 1;
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [],
                    }),
                    { status: 200 },
                );
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-continue"));
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-hardware-refresh"),
            ).toBeInTheDocument();
        });
        const before = preflightCallCount;
        fireEvent.click(screen.getByTestId("setup-hardware-refresh"));
        await waitFor(() => {
            expect(preflightCallCount).toBeGreaterThan(before);
        });
    });

    it("docker step (success state): exposes a Re-check button", async () => {
        // The user installed Docker mid-flow and just wants to
        // re-confirm before continuing. The OK card now carries a
        // Re-check button alongside Continue.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
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
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-recheck")).toBeInTheDocument();
        });
    });

    // Smoke check that the recordedFetch helper compiles + the mocks
    // expose `calls` consistently. Cheap belt-and-braces.
    it("recordedFetch returns the same shared list", () => {
        expect(recordedFetch()).toBe(calls);
    });
});
