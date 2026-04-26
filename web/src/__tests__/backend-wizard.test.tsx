// Phase 13.B.1 — direct tests for the BackendWizardPanel component.
// The integration with BackendsPage is exercised in
// backends-page.test.tsx; these tests target the wizard component
// in isolation so we can cover edge cases (empty preset list, no
// recommended preset, advanced disclosure args preview) without
// having to wire up the full Settings shell.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import {
    BackendWizardPanel,
    type MaterialisedSpec,
} from "../settings/BackendWizardPanel";
import type { PresetsResponse } from "../api/endpoints";

let fetchMock: ReturnType<typeof vi.fn>;

function makePresetsResponse(
    overrides: Partial<PresetsResponse> = {},
): PresetsResponse {
    return {
        purpose: "VoiceSTT",
        detected_vendors: ["nvidia"],
        presets: [
            {
                id: "whisper-faster-cuda",
                purpose: "VoiceSTT",
                inference_backend: "service-whisper-stt",
                name: "faster-whisper (NVIDIA)",
                description: "CTranslate2-based faster-whisper on CUDA.",
                image: "execlaw/service-whisper-cuda:v1",
                container_port: 8000,
                vendor: "nvidia",
                default_args: [],
                fields: [
                    {
                        kind: "model_size",
                        label: "Model size",
                        choices: ["tiny", "base", "small", "medium", "large-v3"],
                        default: "small",
                        arg_template: "--model-size={value}",
                    },
                ],
                recommended: true,
            },
            {
                id: "whisper-cpu",
                purpose: "VoiceSTT",
                inference_backend: "service-whisper-stt",
                name: "Whisper (CPU)",
                description: "CPU-only Whisper.",
                image: "execlaw/service-whisper-cpu:v1",
                container_port: 8000,
                vendor: "cpu",
                default_args: [],
                fields: [
                    {
                        kind: "model_size",
                        label: "Model size",
                        choices: ["tiny", "base", "small"],
                        default: "small",
                        arg_template: "--model-size={value}",
                    },
                ],
                recommended: false,
            },
        ],
        ...overrides,
    };
}

beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("BackendWizardPanel", () => {
    it("preselects the recommended preset on mount", async () => {
        fetchMock.mockImplementation(async () => {
            return new Response(JSON.stringify(makePresetsResponse()), {
                status: 200,
            });
        });
        const onApply = vi.fn();
        const onSkip = vi.fn();
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={onApply}
                onSkip={onSkip}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        // The recommended (NVIDIA) card has its radio checked.
        const cards = screen.getAllByTestId("backend-wizard-preset-card");
        const cuda = cards.find(
            (c) => c.getAttribute("data-preset-id") === "whisper-faster-cuda",
        )!;
        const radio = cuda.querySelector(
            '[data-testid="backend-wizard-preset-radio"]',
        ) as HTMLInputElement;
        expect(radio.checked).toBe(true);
    });

    it("falls back to the first preset when no CPU + no recommendation exist", async () => {
        // Edge case: a purpose with no CPU preset AND nothing
        // recommended. The wizard's last-resort fallback is
        // presets[0] so the operator at least sees a card selected.
        // Reproduces a hypothetical preset library that has only
        // GPU-specific cards.
        const r = makePresetsResponse();
        r.presets = r.presets.filter((p) => p.vendor !== "cpu");
        for (const p of r.presets) p.recommended = false;
        fetchMock.mockImplementation(
            async () => new Response(JSON.stringify(r), { status: 200 }),
        );
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        const cards = screen.getAllByTestId("backend-wizard-preset-card");
        const first = cards[0].querySelector(
            '[data-testid="backend-wizard-preset-radio"]',
        ) as HTMLInputElement;
        expect(first.checked).toBe(true);
    });

    it("emits a materialised spec carrying the operator's field selection", async () => {
        fetchMock.mockImplementation(
            async () =>
                new Response(JSON.stringify(makePresetsResponse()), {
                    status: 200,
                }),
        );
        const captured: MaterialisedSpec[] = [];
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={(s) => captured.push(s)}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        // Change model size from default 'small' to 'medium'.
        const select = screen.getByTestId(
            "backend-wizard-field",
        ) as HTMLSelectElement;
        fireEvent.change(select, { target: { value: "medium" } });
        // Operator types a GPU id.
        const gpu = screen.getByTestId(
            "backend-wizard-gpu",
        ) as HTMLInputElement;
        fireEvent.change(gpu, { target: { value: "0" } });
        fireEvent.click(screen.getByTestId("backend-wizard-apply"));
        expect(captured).toHaveLength(1);
        const spec = captured[0]!;
        expect(spec.inference_backend).toBe("service-whisper-stt");
        expect(spec.gpu_id).toBe("0");
        expect(spec.model_spec.image).toBe(
            "execlaw/service-whisper-cuda:v1",
        );
        expect(spec.model_spec.args).toContain("--model-size=medium");
        expect(spec.model_spec.container_port).toBe(8000);
    });

    it("advanced disclosure shows materialised args preview", async () => {
        fetchMock.mockImplementation(
            async () =>
                new Response(JSON.stringify(makePresetsResponse()), {
                    status: 200,
                }),
        );
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        // The materialised args preview is rendered inside the
        // advanced disclosure regardless of its open/closed state
        // (jsdom keeps closed-details children in the DOM tree).
        // Default 'small' is substituted into the preview.
        expect(
            screen.getByTestId("backend-wizard-args-preview"),
        ).toHaveTextContent("--model-size=small");
    });

    it("image override flows through to the materialised spec", async () => {
        fetchMock.mockImplementation(
            async () =>
                new Response(JSON.stringify(makePresetsResponse()), {
                    status: 200,
                }),
        );
        const captured: MaterialisedSpec[] = [];
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={(s) => captured.push(s)}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        const override = screen.getByTestId(
            "backend-wizard-image-override",
        ) as HTMLInputElement;
        fireEvent.change(override, {
            target: { value: "registry.example.com/whisper:pinned@sha256:abc" },
        });
        fireEvent.click(screen.getByTestId("backend-wizard-apply"));
        expect(captured[0]!.model_spec.image).toBe(
            "registry.example.com/whisper:pinned@sha256:abc",
        );
    });

    it("skip button calls onSkip", async () => {
        fetchMock.mockImplementation(
            async () =>
                new Response(JSON.stringify(makePresetsResponse()), {
                    status: 200,
                }),
        );
        const onSkip = vi.fn();
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={onSkip}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        fireEvent.click(screen.getByTestId("backend-wizard-skip"));
        expect(onSkip).toHaveBeenCalledTimes(1);
    });

    it("renders 'No GPU' badge when the host has no detected vendors", async () => {
        const empty = makePresetsResponse({ detected_vendors: [] });
        fetchMock.mockImplementation(
            async () =>
                new Response(JSON.stringify(empty), { status: 200 }),
        );
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        const badge = screen.getByTestId("backend-wizard-detected-vendor");
        expect(badge.textContent).toMatch(/no gpu/i);
    });

    it("fetches presets with the right purpose query param", async () => {
        const seenUrls: string[] = [];
        fetchMock.mockImplementation(async (url: string) => {
            seenUrls.push(url);
            return new Response(
                JSON.stringify(makePresetsResponse({ purpose: "VoiceTTS" })),
                { status: 200 },
            );
        });
        render(
            <BackendWizardPanel
                purpose="VoiceTTS"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        expect(
            seenUrls.some((u) => u.includes("/api/admin/backends/presets")),
        ).toBe(true);
        expect(seenUrls.some((u) => u.includes("purpose=VoiceTTS"))).toBe(true);
    });

    it("falls back to the CPU preset when nothing is recommended", async () => {
        // Simulates an AMD-only host (no recommended preset). The
        // wizard's pre-13.B.1-audit behavior was to pick presets[0]
        // (an NVIDIA card), which is the wrong default. The fix
        // prefers the CPU preset when none matches detected hardware.
        const r = makePresetsResponse({ detected_vendors: ["amd"] });
        for (const p of r.presets) p.recommended = false;
        fetchMock.mockImplementation(
            async () => new Response(JSON.stringify(r), { status: 200 }),
        );
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        const cpu = screen
            .getAllByTestId("backend-wizard-preset-card")
            .find(
                (c) => c.getAttribute("data-preset-id") === "whisper-cpu",
            )!;
        const radio = cpu.querySelector(
            '[data-testid="backend-wizard-preset-radio"]',
        ) as HTMLInputElement;
        expect(radio.checked).toBe(true);
    });

    it("preview reflects the image override", async () => {
        fetchMock.mockImplementation(
            async () =>
                new Response(JSON.stringify(makePresetsResponse()), {
                    status: 200,
                }),
        );
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(screen.getByTestId("backend-wizard")).toBeInTheDocument();
        });
        // Without an override the preview shows the preset's default
        // image.
        expect(
            screen.getByTestId("backend-wizard-args-preview"),
        ).toHaveTextContent("execlaw/service-whisper-cuda:v1");
        // Type an override.
        const override = screen.getByTestId(
            "backend-wizard-image-override",
        ) as HTMLInputElement;
        fireEvent.change(override, {
            target: { value: "registry.example.com/whisper:pinned" },
        });
        // Preview reflects the override now — the operator gets visual
        // confirmation before clicking Apply.
        expect(
            screen.getByTestId("backend-wizard-args-preview"),
        ).toHaveTextContent("registry.example.com/whisper:pinned");
    });

    it("surfaces a fetch error when the presets endpoint fails", async () => {
        fetchMock.mockImplementation(async () => {
            return new Response("boom", { status: 500 });
        });
        render(
            <BackendWizardPanel
                purpose="VoiceSTT"
                getToken={() => "tok"}
                onApply={vi.fn()}
                onSkip={vi.fn()}
            />,
        );
        await waitFor(() => {
            expect(
                screen.getByText(/Failed to load presets/i),
            ).toBeInTheDocument();
        });
    });
});
