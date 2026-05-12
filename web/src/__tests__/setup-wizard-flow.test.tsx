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
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "setup-backend-target-select",
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
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });
        // No GPUs detected — the target dropdown should only carry
        // the Remote option.
        const select = screen.getByTestId(
            "setup-backend-target-select",
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
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });

        // Remote is the only target when there are no usable GPUs;
        // the form already shows the URL field. Empty submit →
        // validation error.
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
        await waitFor(() => {
            expect(screen.getByText(/Required/i)).toBeInTheDocument();
        });

        // Garbage URL → validation error.
        fireEvent.change(screen.getByTestId("setup-backend-external-endpoint"), {
            target: { value: "not a url" },
        });
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
        await waitFor(() => {
            expect(screen.getByText(/Doesn't look like a URL/i)).toBeInTheDocument();
        });

        // Real URL + model → success.
        fireEvent.change(screen.getByTestId("setup-backend-external-endpoint"), {
            target: { value: "http://localhost:8000/v1" },
        });
        fireEvent.change(screen.getByTestId("setup-backend-external-model"), {
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
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });
        // Two targets: the NVIDIA GPU + Remote.
        const select = screen.getByTestId(
            "setup-backend-target-select",
        ) as HTMLSelectElement;
        expect(select.options.length).toBe(2);
        expect(select.options[0].textContent).toMatch(/GeForce RTX 4090/);
        // For NVIDIA we don't show the radio picker — we render a
        // "fixed serving method" note instead.
        expect(screen.getByTestId("setup-backend-serving-fixed")).toBeInTheDocument();
        expect(screen.queryByTestId("setup-backend-serving-picker")).toBeNull();
        // Model dropdown should include all five vLLM catalog
        // entries because 24 GB fits the 20 GB Qwen 2.5 32B and
        // the 18 GB Qwen 3.5 27B flagship.
        const modelSelect = screen.getByTestId(
            "setup-backend-model-select",
        ) as HTMLSelectElement;
        expect(modelSelect.options.length).toBe(5);
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
        // gpu_id is the per-vendor ordinal (matches nvidia-docker's
        // `--gpus device=N` semantics), NOT the long GpuId string
        // that bricks create_container.
        expect(body.gpu_id).toBe("0");
        // The supervisor reads `gpu_vendor` from model_spec to pick
        // the device-passthrough strategy.
        expect(body.model_spec.gpu_vendor).toBe("nvidia");
        // Wizard tracks the `nightly` vLLM tag because Qwen 3.5
        // architecture support isn't in any stable cut yet.
        expect(body.model_spec.image).toBe("vllm/vllm-openai:nightly");
        // First option in the dropdown is the locked-decision
        // Qwen 3.5 27B AWQ flagship.
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
            expect(screen.getByTestId("setup-backend-serving-picker")).toBeInTheDocument();
        });
        // Radios for both serving methods.
        const openvino = screen.getByTestId(
            "setup-backend-serving-openvino",
        ) as HTMLInputElement;
        const openarc = screen.getByTestId(
            "setup-backend-serving-openarc",
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
        // gpu_id is the per-vendor ordinal — first Intel card on
        // the host is "0", regardless of how many NVIDIA cards
        // precede it in the list.
        expect(body.gpu_id).toBe("0");
        expect(body.model_spec.gpu_vendor).toBe("intel");
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
            expect(screen.getByTestId("setup-backend-target-select")).toBeInTheDocument();
        });
        const select = screen.getByTestId(
            "setup-backend-target-select",
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
                screen.getByTestId("setup-backend-serving-picker"),
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

    it("post-login resume: existing controller landing on /setup skips account form", async () => {
        // Regression: after a user with an already-created account
        // logs in via /login, the route guard bounces them to
        // /setup (because the wizard wasn't completed). The wizard
        // must NOT show the account-creation form again — it must
        // start at the docker step.
        //
        // We simulate the post-login state by pre-populating the
        // localStorage tokens AuthProvider reads on bootstrap. The
        // /me response identifies the user as a controller, which
        // flips auth.status to "authenticated" once the bootstrap
        // resolves.
        localStorage.setItem("execlaw.access_token", "tok");
        localStorage.setItem("execlaw.refresh_token", "tok");
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
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        // Once auth bootstrap resolves we should see the docker
        // step, not the account form.
        await waitFor(() => {
            expect(screen.getByTestId("setup-docker-ok")).toBeInTheDocument();
        });
        // The account form's submit button must NOT have rendered
        // at any point — it would mean the operator was momentarily
        // staring at the account-creation page.
        expect(screen.queryByTestId("setup-account-form")).toBeNull();
        expect(screen.queryByTestId("setup-account-submit")).toBeNull();
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

    // ---- Apple Silicon / Ollama branch (Phase 14.G) -----------------

    /// Common Apple-Silicon preflight body. Vary `ollama.available` per
    /// test to exercise the install-prompt vs detected-badge branches.
    function applePreflight(ollamaAvailable: boolean) {
        return JSON.stringify({
            // Docker is unreachable on a typical Mac without Docker
            // Desktop — the Apple path doesn't need it, so the form
            // must still surface the Apple GPU as a target.
            docker: { available: false, version: null },
            ollama: ollamaAvailable
                ? {
                      available: true,
                      version: "0.1.43",
                      path: "/opt/homebrew/bin/ollama",
                  }
                : { available: false, version: null, path: null },
            gpus: [
                {
                    id: "0x106b:Apple M3 Pro",
                    vendor: "Apple",
                    pci_vendor_id: "0x106b",
                    pci_device_id: "0x0000",
                    device_files: [],
                    kernel_card_index: 0,
                    model_name: "Apple M3 Pro",
                    memory_mb: 24576,
                },
            ],
        });
    }

    it("apple silicon + ollama installed → shows detected badge and model picker", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(applePreflight(true), { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        // Docker is unavailable on this fixture — wizard surfaces the
        // missing-Docker step. Skip past it (Apple path doesn't need
        // Docker).
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-docker-missing"),
            ).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-skip"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });
        // Two targets: Apple GPU + Remote (no NVIDIA/Intel because
        // dockerAvailable=false, but Apple bypasses that gate).
        const select = screen.getByTestId(
            "setup-backend-target-select",
        ) as HTMLSelectElement;
        expect(select.options.length).toBe(2);
        expect(select.options[0].textContent).toMatch(/Apple M3 Pro/);
        // Ollama is installed → "detected" badge appears, install
        // panel does not.
        expect(
            screen.getByTestId("setup-backend-ollama-detected"),
        ).toBeInTheDocument();
        expect(
            screen.queryByTestId("setup-backend-ollama-install"),
        ).toBeNull();
        // Model picker IS rendered (the Apple-Ollama catalog has
        // 4 entries; on a 24 GB Mac all four fit).
        const modelSelect = screen.getByTestId(
            "setup-backend-model-select",
        ) as HTMLSelectElement;
        expect(modelSelect.options.length).toBe(4);
        // Save is enabled.
        const submit = screen.getByTestId(
            "setup-backend-submit",
        ) as HTMLButtonElement;
        expect(submit.disabled).toBe(false);
    });

    it("apple silicon + ollama missing → shows install panel and disables save", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(applePreflight(false), { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-docker-missing"),
            ).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-skip"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });
        // Apple GPU still appears as a target — the form doesn't
        // hide it just because Ollama isn't installed; instead it
        // surfaces the install panel so the operator knows what
        // to do.
        const select = screen.getByTestId(
            "setup-backend-target-select",
        ) as HTMLSelectElement;
        expect(select.options[0].textContent).toMatch(/Apple M3 Pro/);
        // Install panel renders, detected badge does not.
        expect(
            screen.getByTestId("setup-backend-ollama-install"),
        ).toBeInTheDocument();
        expect(
            screen.queryByTestId("setup-backend-ollama-detected"),
        ).toBeNull();
        // brew install copy is present so the operator can copy/paste.
        expect(
            screen.getByText(/brew install ollama/),
        ).toBeInTheDocument();
        // Model picker is HIDDEN until Ollama is detected — avoids
        // an operator picking a model into a spec the supervisor
        // can't spawn.
        expect(
            screen.queryByTestId("setup-backend-model-select"),
        ).toBeNull();
        // Save is disabled because Ollama isn't available.
        const submit = screen.getByTestId(
            "setup-backend-submit",
        ) as HTMLButtonElement;
        expect(submit.disabled).toBe(true);
    });

    it("apple silicon save emits native-runtime envelope (no image, runtime/binary_hint set)", async () => {
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/setup") return setupResponse();
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/setup/preflight") {
                return new Response(applePreflight(true), { status: 200 });
            }
            if (
                url === "/api/admin/backends/Standard" &&
                init?.method === "PUT"
            ) {
                return new Response(
                    JSON.stringify({
                        purpose: "Standard",
                        inference_backend: "service-ollama",
                        model_spec: {},
                        gpu_id: null,
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
            return new Response("{}", { status: 200 });
        });
        mountWizard();
        await fillAndSubmitAccount();
        await waitFor(() => {
            expect(
                screen.getByTestId("setup-docker-missing"),
            ).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-docker-skip"));
        await waitFor(() => {
            expect(screen.getByTestId("setup-backend-form")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("setup-backend-submit"));
        // Wizard advances to chat shell on success.
        await waitFor(() => {
            expect(screen.getByTestId("chat-shell")).toBeInTheDocument();
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/backends/Standard" &&
                c.init?.method === "PUT",
        )!;
        expect(put).toBeDefined();
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.mode).toBe("managed");
        expect(body.inference_backend).toBe("service-ollama");
        // gpu_id is null — Ollama discovers Metal on its own.
        expect(body.gpu_id).toBeNull();
        // The native-runtime envelope: no `image`, but `runtime`
        // and `binary_hint` set. The supervisor's spec_from_row
        // routes this to NativeServiceController.
        expect(body.model_spec.runtime).toBe("native");
        expect(body.model_spec.binary_hint).toBe("ollama");
        expect(body.model_spec.image).toBeUndefined();
        expect(body.model_spec.container_port).toBe(11434);
        // Default model from the curated catalog (largest that fits).
        expect(body.model_spec.model).toMatch(/^qwen2\.5:/);
        // `serve` is the only CLI arg `ollama serve` needs.
        expect(body.model_spec.args).toEqual(["serve"]);
    });

    // Smoke check that the recordedFetch helper compiles + the mocks
    // expose `calls` consistently. Cheap belt-and-braces.
    it("recordedFetch returns the same shared list", () => {
        expect(recordedFetch()).toBe(calls);
    });
});
