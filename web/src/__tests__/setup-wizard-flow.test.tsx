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
        // No Docker = unified backend with only the "Remote" option.
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-unified")).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "setup-target-select",
        ) as HTMLSelectElement;
        // Only the Remote target — no GPU rows because Docker isn't
        // available.
        expect(select.options.length).toBe(1);
        expect(select.options[0].textContent).toMatch(/Remote/);
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
            expect(screen.getByTestId("setup-backend-unified")).toBeInTheDocument();
        });
        // No GPUs detected — the target dropdown should only carry
        // the Remote option.
        const select = screen.getByTestId(
            "setup-target-select",
        ) as HTMLSelectElement;
        expect(select.options.length).toBe(1);
        expect(select.options[0].textContent).toMatch(/Remote/);
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
            expect(screen.getByTestId("setup-backend-unified")).toBeInTheDocument();
        });

        // Remote is the only target when there are no usable GPUs;
        // the form already shows the URL field. Empty submit →
        // validation error.
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
        await waitFor(() => {
            expect(screen.getByText(/Required/i)).toBeInTheDocument();
        });

        // Garbage URL → validation error.
        fireEvent.change(screen.getByTestId("setup-external-endpoint"), {
            target: { value: "not a url" },
        });
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
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
        fireEvent.click(screen.getByTestId("setup-backend-submit"));

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

    it("nvidia GPU → vLLM is the only serving method, model dropdown filters by VRAM", async () => {
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
                                model_name: "GeForce RTX 4090",
                                memory_mb: 24_576,
                            },
                        ],
                    }),
                    { status: 200 },
                );
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
            expect(screen.getByTestId("setup-backend-unified")).toBeInTheDocument();
        });
        // Two targets: the NVIDIA GPU + Remote.
        const select = screen.getByTestId(
            "setup-target-select",
        ) as HTMLSelectElement;
        expect(select.options.length).toBe(2);
        expect(select.options[0].textContent).toMatch(/GeForce RTX 4090/);
        // For NVIDIA we don't show the radio picker — we render a
        // "fixed serving method" note instead.
        expect(screen.getByTestId("setup-serving-fixed")).toBeInTheDocument();
        expect(screen.queryByTestId("setup-serving-picker")).toBeNull();
        // Model dropdown should include all three vLLM catalog
        // entries because 24 GB fits the 18 GB flagship.
        const modelSelect = screen.getByTestId(
            "setup-model-select",
        ) as HTMLSelectElement;
        expect(modelSelect.options.length).toBe(3);
        // Save → PUT with vLLM image + chosen model in args.
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
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
        expect(body.inference_backend).toBe("service-vllm");
        expect(body.gpu_id).toBe("0x10de:0x2684");
        expect(body.model_spec.image).toBe("vllm/vllm-openai:v0.6.2");
        // The first option in the dropdown is the 27B AWQ flagship
        // — that's what gets saved on default.
        expect(body.model_spec.args).toContain(
            "--model=QuantTrio/Qwen3.5-27B-AWQ",
        );
    });

    it("intel arc GPU → OpenVINO + OpenArc radios + INT4 catalog", async () => {
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
                                id: "0x8086:0xe20b",
                                vendor: "Intel",
                                pci_vendor_id: "0x8086",
                                pci_device_id: "0xe20b",
                                model_name: "Arc A770",
                                memory_mb: 16_384,
                            },
                        ],
                    }),
                    { status: 200 },
                );
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
            expect(screen.getByTestId("setup-serving-picker")).toBeInTheDocument();
        });
        // Radios for both serving methods.
        const openvino = screen.getByTestId(
            "setup-serving-openvino",
        ) as HTMLInputElement;
        const openarc = screen.getByTestId(
            "setup-serving-openarc",
        ) as HTMLInputElement;
        expect(openvino.checked).toBe(true);
        // Switch to OpenArc.
        fireEvent.click(openarc);
        expect(openarc.checked).toBe(true);
        // Save → PUT with the openarc plugin id.
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
        await waitFor(() => {
            expect(screen.getByTestId("chat-shell")).toBeInTheDocument();
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/backends/Standard" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.inference_backend).toBe("service-openarc");
        expect(body.gpu_id).toBe("0x8086:0xe20b");
    });

    it("multi-GPU host: dropdown lists every (GPU, serving) combination + Remote", async () => {
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
                                model_name: "GeForce RTX 4090",
                                memory_mb: 24_576,
                            },
                            {
                                id: "0x8086:0xe20b",
                                vendor: "Intel",
                                pci_vendor_id: "0x8086",
                                pci_device_id: "0xe20b",
                                model_name: "Arc A770",
                                memory_mb: 16_384,
                            },
                        ],
                    }),
                    { status: 200 },
                );
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
            expect(screen.getByTestId("setup-target-select")).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "setup-target-select",
        ) as HTMLSelectElement;
        // 1 NVIDIA + 1 Intel + 1 Remote = 3 options. (Intel +
        // OpenVINO/OpenArc collapses into one target row; the
        // serving-method radios live below.)
        expect(select.options.length).toBe(3);
        const labels = Array.from(select.options).map((o) => o.textContent ?? "");
        expect(labels.some((l) => /GeForce RTX 4090/.test(l))).toBe(true);
        expect(labels.some((l) => /Arc A770/.test(l))).toBe(true);
        expect(labels.some((l) => /Remote/.test(l))).toBe(true);
        // Switch to the Intel Arc target — radios appear.
        fireEvent.change(select, { target: { value: "1" } });
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-serving-picker"),
            ).toBeInTheDocument();
        });
    });

    it("hardware summary uses model_name SKU instead of raw PCI string", async () => {
        // The user-reported bug: the badge rendered as
        // "NVIDIA (0x10de:PCI\VEN_10DE&DEV_2230&SUBSYS_…)" — multi
        // line, overflowing. With model_name resolved we should
        // see clean SKU + memory text instead.
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(
                    JSON.stringify({
                        docker: { available: true, version: "24.0.7" },
                        gpus: [
                            {
                                id: "0x10de:0x2230",
                                vendor: "Nvidia",
                                pci_vendor_id: "0x10de",
                                pci_device_id: "0x2230",
                                model_name: "RTX A6000",
                                memory_mb: 49_152,
                            },
                        ],
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
                screen.getByTestId("setup-hardware-summary"),
            ).toBeInTheDocument();
        });
        const badge = screen.getByTestId("setup-hardware-gpu");
        expect(badge.textContent).toMatch(/RTX A6000/);
        expect(badge.textContent).toMatch(/48\.0 GB/);
        // Critical: the badge must NOT contain the multi-line PNP
        // string the user complained about.
        expect(badge.textContent).not.toMatch(/PCI\\VEN_/);
        expect(badge.textContent).not.toMatch(/SUBSYS/);
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
