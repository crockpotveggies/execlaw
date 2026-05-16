// 2026-05-16 — coverage for the Skills page's standardized layout
// + the new "+ New skill" button + modal added alongside it. Backs
// the manual-authoring flow that complements the agent's
// `skills.create` tool path.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
    act,
    fireEvent,
    render,
    screen,
    waitFor,
} from "@testing-library/react";
import { AuthProvider } from "../auth/AuthContext";
import { SkillsPage } from "../skills/SkillsPage";

let fetchMock: ReturnType<typeof vi.fn>;

const meResponse = (role: "controller" | "operator" = "controller") =>
    new Response(
        JSON.stringify({
            user_id: "u-1",
            username: "u",
            display_name: "U",
            email: null,
            role,
            last_login_at: null,
        }),
        { status: 200 },
    );

function listResponse(skills: unknown[] = []) {
    return new Response(JSON.stringify({ skills }), { status: 200 });
}

function configResponse() {
    return new Response(
        JSON.stringify({
            auto_capture_enabled: false,
            auto_capture_min_tool_calls: 5,
            auto_capture_dry_run: false,
            reuse_update_enabled: false,
            updated_at: 0,
        }),
        { status: 200 },
    );
}

function proposalsResponse() {
    return new Response(JSON.stringify({ proposals: [] }), { status: 200 });
}

function detailResponseFor(name: string) {
    return new Response(
        JSON.stringify({
            name,
            description: "seeded",
            state: "trial",
            registration_kind: "authored",
            source: "admin:u-1",
            owning_plugin_id: null,
            current_version: 1,
            body_md: "# body",
            frontmatter_json: "{}",
            authored_by: "admin:u-1",
            authored_at: 0,
            created_at: 0,
            updated_at: 0,
            archived_at: null,
            resource_paths: [],
        }),
        { status: 200 },
    );
}

function mountPage() {
    return render(
        <AuthProvider>
            <SkillsPage />
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

describe("SkillsPage standardized layout", () => {
    it("renders the New skill button in the toolbar when controller", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("controller");
            if (url === "/api/admin/skills/config") return configResponse();
            if (url.startsWith("/api/admin/skills")) return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        // Toolbar button is the primary affordance — and the empty-
        // state CTA mirrors it so the operator sees the path no
        // matter where their eye lands. Wait for the empty-state CTA
        // (which renders after the async list fetch resolves) — the
        // toolbar button is up immediately but the empty state only
        // appears once `skills` transitions from null to [].
        await waitFor(() => {
            expect(
                screen.getByTestId("skills-empty-new-btn"),
            ).toBeInTheDocument();
        });
        expect(screen.getByTestId("skills-new-btn")).toBeInTheDocument();
    });

    it("hides the New button entirely for non-controller users", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse("operator");
            if (url === "/api/admin/skills/config") return configResponse();
            if (url.startsWith("/api/admin/skills")) return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => {
            expect(screen.getByTestId("skills-toolbar")).toBeInTheDocument();
        });
        expect(screen.queryByTestId("skills-new-btn")).toBeNull();
        expect(screen.queryByTestId("skills-empty-new-btn")).toBeNull();
    });

    it("uses the standardized scaffolding classes on the page wrapper", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/skills/config") return configResponse();
            if (url.startsWith("/api/admin/skills")) return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        const { container } = mountPage();
        // Pre-2026-05-16 the page rendered with raw d-flex utilities
        // and inline widths and bled past the viewport. The scaffold
        // contract is `.execlaw-page.execlaw-skills` so detail/list
        // children get bounded layout.
        await waitFor(() => {
            expect(
                container.querySelector(".execlaw-page.execlaw-skills"),
            ).toBeTruthy();
        });
    });
});

describe("SkillsPage — new-skill modal", () => {
    it("opens the modal on toolbar click and validates name format", async () => {
        fetchMock.mockImplementation(async (url: string) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/skills/config") return configResponse();
            if (url.startsWith("/api/admin/skills")) return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() =>
            screen.getByTestId("skills-new-btn"),
        );
        fireEvent.click(screen.getByTestId("skills-new-btn"));
        const modal = await screen.findByTestId("skills-new-modal");
        expect(modal).toBeInTheDocument();
        // Submit disabled with empty fields.
        const submit = screen.getByTestId(
            "skills-new-create",
        ) as HTMLButtonElement;
        expect(submit.disabled).toBe(true);
        // Bad name shape (no slash) keeps it disabled.
        fireEvent.change(screen.getByTestId("skills-new-name"), {
            target: { value: "NoSlash" },
        });
        fireEvent.change(screen.getByTestId("skills-new-description"), {
            target: { value: "x" },
        });
        fireEvent.change(screen.getByTestId("skills-new-body"), {
            target: { value: "x" },
        });
        expect(submit.disabled).toBe(true);
        // Canonical shape enables submit.
        fireEvent.change(screen.getByTestId("skills-new-name"), {
            target: { value: "test/web-browsing" },
        });
        expect(submit.disabled).toBe(false);
    });

    it("POSTs to /api/admin/skills and refreshes the list on success", async () => {
        const calls: Array<{ url: string; init?: RequestInit }> = [];
        let listCallCount = 0;
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            calls.push({ url, init });
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/skills/config") return configResponse();
            if (url.startsWith("/api/admin/skills/proposals"))
                return proposalsResponse();
            if (
                url === "/api/admin/skills" &&
                (init?.method ?? "GET") === "GET"
            ) {
                listCallCount += 1;
                // Initial fetch returns empty; post-create refresh
                // returns the new skill so the detail can resolve.
                return listResponse(
                    listCallCount > 1
                        ? [
                              {
                                  name: "test/web-browsing",
                                  description: "Use search + fetch tools.",
                                  state: "trial",
                                  version: 1,
                                  registration_kind: "authored",
                                  source: "admin:u-1",
                                  owning_plugin_id: null,
                                  updated_at: 0,
                              },
                          ]
                        : [],
                );
            }
            if (
                url === "/api/admin/skills" &&
                init?.method === "POST"
            ) {
                return detailResponseFor("test/web-browsing");
            }
            if (url.startsWith("/api/admin/skills/")) {
                // detail GET after refresh
                return detailResponseFor("test/web-browsing");
            }
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => screen.getByTestId("skills-new-btn"));
        fireEvent.click(screen.getByTestId("skills-new-btn"));
        await screen.findByTestId("skills-new-modal");

        fireEvent.change(screen.getByTestId("skills-new-name"), {
            target: { value: "test/web-browsing" },
        });
        fireEvent.change(screen.getByTestId("skills-new-description"), {
            target: { value: "Use search + fetch tools." },
        });
        fireEvent.change(screen.getByTestId("skills-new-body"), {
            target: { value: "# Web Browsing\n\nSearch first." },
        });

        await act(async () => {
            fireEvent.click(screen.getByTestId("skills-new-create"));
        });

        // Modal closes + list refresh fires.
        await waitFor(() => {
            expect(screen.queryByTestId("skills-new-modal")).toBeNull();
        });

        // The POST landed with the canonical wire shape.
        const post = calls.find(
            (c) =>
                c.url === "/api/admin/skills" && c.init?.method === "POST",
        );
        expect(post).toBeTruthy();
        const sent = JSON.parse(post!.init!.body as string);
        expect(sent.name).toBe("test/web-browsing");
        expect(sent.description).toBe("Use search + fetch tools.");
        expect(sent.body_md).toContain("Web Browsing");
        // Frontmatter NOT shipped when the advanced toggle is off.
        expect(sent.frontmatter_json).toBeUndefined();

        // List re-fetched at least twice (initial mount + post-create).
        expect(listCallCount).toBeGreaterThanOrEqual(2);
    });

    it("ships the custom frontmatter only when the advanced toggle is on", async () => {
        const posts: RequestInit[] = [];
        fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
            if (url === "/api/admin/me") return meResponse();
            if (url === "/api/admin/skills/config") return configResponse();
            if (
                url === "/api/admin/skills" &&
                init?.method === "POST"
            ) {
                posts.push(init);
                return detailResponseFor("test/with-frontmatter");
            }
            if (url.startsWith("/api/admin/skills"))
                return listResponse([]);
            return new Response("{}", { status: 200 });
        });
        mountPage();
        await waitFor(() => screen.getByTestId("skills-new-btn"));
        fireEvent.click(screen.getByTestId("skills-new-btn"));
        await screen.findByTestId("skills-new-modal");

        fireEvent.change(screen.getByTestId("skills-new-name"), {
            target: { value: "test/with-frontmatter" },
        });
        fireEvent.change(screen.getByTestId("skills-new-description"), {
            target: { value: "x" },
        });
        fireEvent.change(screen.getByTestId("skills-new-body"), {
            target: { value: "x" },
        });
        // Flip the advanced toggle and edit the frontmatter.
        fireEvent.click(
            screen.getByTestId("skills-new-advanced-toggle"),
        );
        fireEvent.change(screen.getByTestId("skills-new-frontmatter"), {
            target: { value: '{"category":"web"}' },
        });

        await act(async () => {
            fireEvent.click(screen.getByTestId("skills-new-create"));
        });

        await waitFor(() => expect(posts.length).toBe(1));
        const sent = JSON.parse(posts[0].body as string);
        expect(sent.frontmatter_json).toBe('{"category":"web"}');
    });
});
