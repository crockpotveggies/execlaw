import { describe, expect, it, vi } from "vitest";
import {
    act,
    fireEvent,
    render,
    screen,
    waitFor,
    within,
} from "@testing-library/react";
import { Composer } from "../chat/Composer";
import type { SkillListEntry } from "../api/endpoints";

/// Build a SkillListEntry suitable for the picker tests. We don't
/// care about the registration_kind / source / updated_at fields
/// on the SPA side beyond name + description, but the type forces
/// them; defaulting here keeps each test terse.
function fakeSkill(name: string, description: string): SkillListEntry {
    return {
        name,
        description,
        state: "stable",
        version: 1,
        registration_kind: "authored",
        source: "test",
        owning_plugin_id: null,
        updated_at: 0,
    };
}

describe("Composer", () => {
    it("send button is disabled when input is empty", () => {
        render(<Composer onSend={() => {}} />);
        const send = screen.getByTestId("composer-send") as HTMLButtonElement;
        expect(send.disabled).toBe(true);
    });

    it("calls onSend with the trimmed text on submit", async () => {
        const onSend = vi.fn().mockResolvedValue(undefined);
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "  hello  " } });
        fireEvent.submit(input.closest("form")!);
        expect(onSend).toHaveBeenCalledWith("hello", [], []);
    });

    it("Enter submits, Shift+Enter does not", () => {
        const onSend = vi.fn();
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "msg" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
        expect(onSend).not.toHaveBeenCalled();
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("msg", [], []);
    });

    it("respects an external `disabled` prop", () => {
        render(<Composer onSend={() => {}} disabled />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        const send = screen.getByTestId("composer-send") as HTMLButtonElement;
        expect(input.disabled).toBe(true);
        expect(send.disabled).toBe(true);
    });

    /// 2026-04-28 regression: hitting Enter used to disable the
    /// textarea via `submitting=true`, which blurred the element
    /// (disabled inputs lose focus). The user lost their place
    /// every time they sent a message. The fix only disables on
    /// the explicit `disabled` prop; the submit guard inside
    /// `submit()` still prevents double-sends.
    it("textarea stays focused (and editable) across an Enter-driven submit", async () => {
        // Long-running onSend simulates the agent streaming a reply
        // — the textarea must NOT lock the user out during that
        // window.
        type Resolver = () => void;
        const resolverHolder: { fn: Resolver | null } = { fn: null };
        const onSend = vi.fn(
            (): Promise<void> =>
                new Promise<void>((resolve) => {
                    resolverHolder.fn = resolve as Resolver;
                }),
        );
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        input.focus();
        expect(document.activeElement).toBe(input);

        fireEvent.change(input, { target: { value: "first" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledWith("first", [], []);

        // Mid-await: textarea should still be focused AND editable
        // so the operator can compose the next thought while the
        // agent streams.
        expect(document.activeElement).toBe(input);
        expect(input.disabled).toBe(false);
        fireEvent.change(input, { target: { value: "second draft" } });
        expect(input.value).toBe("second draft");

        // Resolve the in-flight send — composer flips out of
        // submitting; nothing else should change because the
        // textarea was never disabled in the first place.
        resolverHolder.fn?.();
        await Promise.resolve();
        expect(document.activeElement).toBe(input);
        expect(input.disabled).toBe(false);
    });

    /// Hitting Enter twice in quick succession should NOT fire
    /// onSend twice — the in-flight submit guard catches the
    /// second one. Belt-and-suspenders for the no-disable change
    /// above; without the guard, hot-typing would queue duplicate
    /// turns server-side.
    it("guards against double-submits while a send is in flight", () => {
        type Resolver = () => void;
        const resolverHolder: { fn: Resolver | null } = { fn: null };
        const onSend = vi.fn(
            (): Promise<void> =>
                new Promise<void>((resolve) => {
                    resolverHolder.fn = resolve as Resolver;
                }),
        );
        render(<Composer onSend={onSend} />);
        const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "first" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        // Without resolving onSend, hammer Enter again. The guard
        // should suppress the second submit.
        fireEvent.change(input, { target: { value: "second" } });
        fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
        expect(onSend).toHaveBeenCalledTimes(1);
        resolverHolder.fn?.();
    });

    // ---- skill picker (composer `+` menu, second item) ----------

    it("shows the `+` button when getSkills is wired even without multimodal", () => {
        render(
            <Composer
                onSend={() => {}}
                getSkills={async () => [fakeSkill("test/foo", "desc")]}
            />,
        );
        // The trigger appears purely on the strength of getSkills —
        // operators on text-only backends still need the picker.
        expect(screen.getByTestId("composer-attach-trigger")).toBeTruthy();
    });

    it("hides the `+` button when neither multimodal nor getSkills is supplied", () => {
        render(<Composer onSend={() => {}} />);
        expect(screen.queryByTestId("composer-attach-trigger")).toBeNull();
    });

    it("renders both menu items when multimodal AND getSkills are wired", () => {
        render(
            <Composer
                onSend={() => {}}
                multimodal
                getSkills={async () => [fakeSkill("test/foo", "desc")]}
            />,
        );
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        expect(screen.getByTestId("composer-attach-photo")).toBeTruthy();
        expect(screen.getByTestId("composer-attach-skill")).toBeTruthy();
    });

    it("only renders the skill menu item when multimodal is off", () => {
        render(
            <Composer
                onSend={() => {}}
                getSkills={async () => [fakeSkill("test/foo", "desc")]}
            />,
        );
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        expect(screen.queryByTestId("composer-attach-photo")).toBeNull();
        expect(screen.getByTestId("composer-attach-skill")).toBeTruthy();
    });

    it("lazy-loads the skill list on first picker open and caches across opens", async () => {
        const getSkills = vi
            .fn<() => Promise<SkillListEntry[]>>()
            .mockResolvedValue([
                fakeSkill("test/alpha", "alpha desc"),
                fakeSkill("test/beta", "beta desc"),
            ]);
        render(<Composer onSend={() => {}} getSkills={getSkills} />);
        // Initial render: no fetch yet (lazy).
        expect(getSkills).not.toHaveBeenCalled();

        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        await act(async () => {
            fireEvent.click(screen.getByTestId("composer-attach-skill"));
        });
        expect(getSkills).toHaveBeenCalledTimes(1);
        await waitFor(() => {
            expect(
                screen.getAllByTestId("composer-skill-picker-item"),
            ).toHaveLength(2);
        });

        // Close + reopen + dive back into the picker — must NOT
        // re-fetch (cached for the Composer's lifetime).
        fireEvent.click(screen.getByTestId("composer-skill-picker-back"));
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        fireEvent.click(screen.getByTestId("composer-attach-skill"));
        expect(getSkills).toHaveBeenCalledTimes(1);
    });

    it("toggles a skill on click and renders a chip; chip remove un-stages it", async () => {
        const getSkills = async () => [
            fakeSkill("test/foo", "foo description"),
            fakeSkill("test/bar", "bar description"),
        ];
        render(<Composer onSend={() => {}} getSkills={getSkills} />);
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        await act(async () => {
            fireEvent.click(screen.getByTestId("composer-attach-skill"));
        });
        await waitFor(() => {
            expect(
                screen.getAllByTestId("composer-skill-picker-item"),
            ).toHaveLength(2);
        });
        const items = screen.getAllByTestId("composer-skill-picker-item");
        const fooItem = items.find(
            (el) => el.getAttribute("data-skill-name") === "test/foo",
        )!;
        fireEvent.click(fooItem);

        // Chip appears in the staged-attachments row.
        const chip = await screen.findByTestId("composer-skill-chip");
        expect(chip.getAttribute("data-skill-name")).toBe("test/foo");

        // Click the picker item again to toggle off — chip vanishes.
        fireEvent.click(fooItem);
        expect(screen.queryByTestId("composer-skill-chip")).toBeNull();

        // Re-stage and remove via the chip's `x` button.
        fireEvent.click(fooItem);
        await screen.findByTestId("composer-skill-chip");
        fireEvent.click(screen.getByTestId("composer-skill-chip-remove"));
        expect(screen.queryByTestId("composer-skill-chip")).toBeNull();
    });

    it("submits the staged skill names to onSend and clears them after send", async () => {
        const onSend = vi.fn().mockResolvedValue(undefined);
        const getSkills = async () => [
            fakeSkill("test/alpha", "alpha"),
            fakeSkill("test/beta", "beta"),
        ];
        render(<Composer onSend={onSend} getSkills={getSkills} />);
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        await act(async () => {
            fireEvent.click(screen.getByTestId("composer-attach-skill"));
        });
        await waitFor(() => {
            expect(
                screen.getAllByTestId("composer-skill-picker-item"),
            ).toHaveLength(2);
        });
        const items = screen.getAllByTestId("composer-skill-picker-item");
        // Pick beta first, then alpha — order of staging matters
        // (matches operator selection order, which the server then
        // reflects in prepend ordering).
        fireEvent.click(
            items.find(
                (el) => el.getAttribute("data-skill-name") === "test/beta",
            )!,
        );
        fireEvent.click(
            items.find(
                (el) => el.getAttribute("data-skill-name") === "test/alpha",
            )!,
        );

        const input = screen.getByTestId(
            "composer-input",
        ) as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: "do the thing" } });
        await act(async () => {
            fireEvent.submit(input.closest("form")!);
        });
        expect(onSend).toHaveBeenCalledWith(
            "do the thing",
            [],
            ["test/beta", "test/alpha"],
        );
        // Per-turn semantics: chips clear after send.
        expect(screen.queryByTestId("composer-skill-chip")).toBeNull();
    });

    it("surfaces an inline error when getSkills rejects, without crashing the menu", async () => {
        const getSkills = vi
            .fn<() => Promise<SkillListEntry[]>>()
            .mockRejectedValue(new Error("boom: 500"));
        render(<Composer onSend={() => {}} getSkills={getSkills} />);
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        await act(async () => {
            fireEvent.click(screen.getByTestId("composer-attach-skill"));
        });
        const status = await screen.findByTestId(
            "composer-skill-picker-error",
        );
        expect(within(status).getByText(/boom: 500/)).toBeTruthy();
    });

    it("shows an empty-state when the backend returns zero skills", async () => {
        const getSkills = vi
            .fn<() => Promise<SkillListEntry[]>>()
            .mockResolvedValue([]);
        render(<Composer onSend={() => {}} getSkills={getSkills} />);
        fireEvent.click(screen.getByTestId("composer-attach-trigger"));
        await act(async () => {
            fireEvent.click(screen.getByTestId("composer-attach-skill"));
        });
        await screen.findByTestId("composer-skill-picker-empty");
    });
});
