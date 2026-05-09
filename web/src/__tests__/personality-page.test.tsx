// Tests for the Settings → Personality page (Phase 9, §5.5).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PersonalityPage } from "../settings/PersonalityPage";
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

function defaultRow() {
    return {
        scope_kind: "default",
        scope_ref: "",
        display_name: "execlaw",
        role: "Personal assistant",
        tone: "",
        communication_style: "",
        initiative: "",
        about_agent: "",
        about_controller: "",
        custom_instructions: "",
        voice_id: "bf_emma",
        override_fields: [
            "display_name",
            "role",
            "tone",
            "communication_style",
            "initiative",
            "about_agent",
            "about_controller",
            "custom_instructions",
            "voice_id",
        ],
        version: 1,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    };
}

function listResponse(overrides: Array<Record<string, unknown>> = []) {
    return new Response(
        JSON.stringify({
            default: defaultRow(),
            overrides,
        }),
        { status: 200 },
    );
}

function previewResponse(prompt: string) {
    return new Response(
        JSON.stringify({
            conversation_id: "",
            system_prompt: prompt,
        }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <PersonalityPage />
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

describe("PersonalityPage", () => {
    it("loads the seeded default row into the form and shows the preview", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/personality") return listResponse();
            if (url === "/api/admin/personality/preview")
                return previewResponse("# Identity\nName: execlaw\n");
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                (screen.getByTestId("personality-display-name") as HTMLInputElement)
                    .value,
            ).toBe("execlaw");
        });
        expect(
            (screen.getByTestId("personality-voice-id") as HTMLInputElement).value,
        ).toBe("bf_emma");
        // Preview pane shows the rendered prompt.
        expect(screen.getByTestId("personality-preview")).toBeInTheDocument();
        expect(screen.getByText(/Name: execlaw/)).toBeInTheDocument();
    });

    it("PUTs the right body when saving — voice_id null when blank", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (
                url === "/api/admin/personality" &&
                (!init || init.method === undefined || init.method === "GET")
            )
                return listResponse();
            if (url === "/api/admin/personality/preview")
                return previewResponse("# Identity\nName: execlaw\n");
            if (
                url === "/api/admin/personality/default" &&
                init?.method === "PUT"
            ) {
                const updated = defaultRow();
                updated.display_name = "Earl";
                updated.tone = "Concise";
                updated.voice_id = null as unknown as string;
                updated.version = 2;
                return new Response(JSON.stringify(updated), { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("personality-display-name")).toBeInTheDocument();
        });
        fireEvent.change(screen.getByTestId("personality-display-name"), {
            target: { value: "Earl" },
        });
        fireEvent.change(screen.getByTestId("personality-tone"), {
            target: { value: "Concise" },
        });
        // Blank out voice_id — null wire-format expected.
        fireEvent.change(screen.getByTestId("personality-voice-id"), {
            target: { value: "" },
        });
        fireEvent.click(screen.getByTestId("personality-save"));
        await waitFor(() => {
            expect(
                calls.some(
                    (c) =>
                        c.url === "/api/admin/personality/default" &&
                        c.init?.method === "PUT",
                ),
            ).toBe(true);
        });
        const put = calls.find(
            (c) =>
                c.url === "/api/admin/personality/default" &&
                c.init?.method === "PUT",
        )!;
        const body = JSON.parse((put.init?.body as string) ?? "{}");
        expect(body.display_name).toBe("Earl");
        expect(body.tone).toBe("Concise");
        expect(body.voice_id).toBeNull();
        // Default scope: override_fields not required (server fills in
        // every field for default scope), but the SPA may omit it.
    });

    it("renders the empty hint when no overrides exist", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/personality") return listResponse();
            if (url === "/api/admin/personality/preview")
                return previewResponse("# Identity\nName: execlaw\n");
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(
                screen.getByTestId("personality-overrides-card"),
            ).toBeInTheDocument();
        });
        expect(
            screen.getByText(/no overrides on file/i),
        ).toBeInTheDocument();
    });

    it("renders + drops a conversation override", async () => {
        // Stub `confirm` so the delete branch proceeds.
        const confirmSpy = vi
            .spyOn(window, "confirm")
            .mockImplementation(() => true);

        const initialOverride = {
            scope_kind: "conversation",
            scope_ref: "conv-pirate",
            display_name: "",
            role: "",
            tone: "Pirate",
            communication_style: "",
            initiative: "",
            about_agent: "",
            about_controller: "",
            custom_instructions: "",
            voice_id: null,
            override_fields: ["tone"],
            version: 1,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };

        let deleted = false;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/personality")
                return listResponse(deleted ? [] : [initialOverride]);
            if (url === "/api/admin/personality/preview")
                return previewResponse("# Identity\nName: execlaw\n");
            if (
                url === "/api/admin/personality/conversation/conv-pirate" &&
                init?.method === "DELETE"
            ) {
                deleted = true;
                return new Response("", { status: 200 });
            }
            return new Response("{}", { status: 200 });
        });

        mountPage();
        await waitFor(() => {
            expect(screen.getByText("conv-pirate")).toBeInTheDocument();
        });
        // The summary text should include the field name (filter strips voice_id only).
        expect(screen.getByText(/overrides tone/)).toBeInTheDocument();

        fireEvent.click(screen.getByTestId("personality-override-delete"));
        await waitFor(() => {
            expect(screen.queryByText("conv-pirate")).toBeNull();
        });
        expect(
            screen.getByText(/no overrides on file/i),
        ).toBeInTheDocument();

        confirmSpy.mockRestore();
    });
});
