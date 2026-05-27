// Slice G (2026-05-26) — pure-helper tests for the plugin
// branch-suggestion client.
//
// `renderBranchSuggestion` is a pure substitution helper; the test
// pins the contract that:
//   1. `{placeholder}` chips are replaced by the supplied overrides.
//   2. Keys the caller didn't override fall back to the
//      suggestion's manifest-declared defaults.
//   3. Templates with multiple placeholders all resolve.
//   4. A placeholder that exists in the template but is missing
//      from BOTH overrides and defaults stays as literal `{name}`
//      so the operator sees what they forgot to fill in.
//
// `filterBranchSuggestions` is the picker-side gate. Test pins:
//   1. `event_kind` mismatch hides the suggestion.
//   2. `when_active` substring miss hides the suggestion.
//   3. `when_active` substring hit (or absent) admits it.

import { describe, expect, it } from "vitest";
import {
    type BranchSuggestion,
    filterBranchSuggestions,
    renderBranchSuggestion,
} from "../api/automations";

function suggestion(overrides: Partial<BranchSuggestion> = {}): BranchSuggestion {
    return {
        event_kind: "chat.prompt",
        when_active: undefined,
        display_name: "test",
        description: "",
        template: "1 == 1",
        defaults: {},
        source_plugin_id: "test-plugin",
        source_plugin_version: "0.0.0",
        ...overrides,
    };
}

describe("renderBranchSuggestion", () => {
    it("substitutes placeholder chips with override values", () => {
        const s = suggestion({
            template: 'event.payload.channel_meta.group_name == "{group_name}"',
        });
        expect(renderBranchSuggestion(s, { group_name: "Engineering" })).toBe(
            'event.payload.channel_meta.group_name == "Engineering"',
        );
    });

    it("falls back to manifest defaults for keys not overridden", () => {
        const s = suggestion({
            template: 'event.payload.channel_meta.group_name == "{group_name}"',
            defaults: { group_name: "general" },
        });
        // No override → default lands as-is.
        expect(renderBranchSuggestion(s)).toBe(
            'event.payload.channel_meta.group_name == "general"',
        );
        // Override beats default.
        expect(renderBranchSuggestion(s, { group_name: "ops" })).toBe(
            'event.payload.channel_meta.group_name == "ops"',
        );
    });

    it("resolves multiple placeholders in one template", () => {
        const s = suggestion({
            template:
                'event.payload.text.to_lower().contains("{keyword}") && {min_priority}',
            defaults: { keyword: "urgent", min_priority: "true" },
        });
        expect(renderBranchSuggestion(s)).toBe(
            'event.payload.text.to_lower().contains("urgent") && true',
        );
        expect(
            renderBranchSuggestion(s, {
                keyword: "p0",
                min_priority: "false",
            }),
        ).toBe('event.payload.text.to_lower().contains("p0") && false');
    });

    it("leaves unresolved placeholders literal so operators see what's missing", () => {
        // A typo'd manifest (placeholder in template but no default
        // and no override) should produce visibly broken output, not
        // silently strip the chip.
        const s = suggestion({
            template: 'event.payload.x == "{forgot_to_default}"',
        });
        expect(renderBranchSuggestion(s)).toBe(
            'event.payload.x == "{forgot_to_default}"',
        );
    });
});

describe("filterBranchSuggestions", () => {
    const slackOnly = suggestion({
        event_kind: "chat.prompt",
        when_active: 'event.payload.channel == "slack"',
        display_name: "slack",
    });
    const waOnly = suggestion({
        event_kind: "chat.prompt",
        when_active: 'event.payload.channel == "whatsapp"',
        display_name: "whatsapp",
    });
    const universal = suggestion({
        event_kind: "chat.prompt",
        when_active: undefined,
        display_name: "universal",
    });
    const otherKind = suggestion({
        event_kind: "calendar.event.starting_soon",
        display_name: "calendar",
    });
    const all = [slackOnly, waOnly, universal, otherKind];

    it("filters out suggestions whose event_kind doesn't match the active trigger", () => {
        // Different-kind suggestion (calendar.*) is dropped even
        // when context is broad; remaining three are chat.prompt
        // suggestions filtered further by when_active.
        const out = filterBranchSuggestions(
            all,
            "chat.prompt",
            'event.payload.channel == "slack"',
        );
        expect(out.map((s) => s.display_name)).not.toContain("calendar");
        // Different-kind context surfaces zero results.
        const wrongKind = filterBranchSuggestions(
            all,
            "some.other.kind",
            'event.payload.channel == "slack"',
        );
        expect(wrongKind).toEqual([]);
    });

    it("hides slack-narrow suggestions until the operator narrowed to slack", () => {
        const empty = filterBranchSuggestions(all, "chat.prompt", "");
        // Empty context: universal-only.
        expect(empty.map((s) => s.display_name)).toContain("universal");
        expect(empty.map((s) => s.display_name)).not.toContain("slack");
        expect(empty.map((s) => s.display_name)).not.toContain("whatsapp");
    });

    it("admits slack suggestions once the slack narrow is in scope", () => {
        const slack = filterBranchSuggestions(
            all,
            "chat.prompt",
            'event.payload.channel == "slack"',
        );
        expect(slack.map((s) => s.display_name)).toContain("slack");
        expect(slack.map((s) => s.display_name)).toContain("universal");
        expect(slack.map((s) => s.display_name)).not.toContain("whatsapp");
    });

    it("admits whatsapp suggestions once the wa narrow is in scope", () => {
        const wa = filterBranchSuggestions(
            all,
            "chat.prompt",
            'event.payload.channel == "whatsapp"',
        );
        expect(wa.map((s) => s.display_name)).toContain("whatsapp");
        expect(wa.map((s) => s.display_name)).not.toContain("slack");
    });

    it("admits universal suggestions regardless of context", () => {
        const empty = filterBranchSuggestions(all, "chat.prompt", "");
        const narrowed = filterBranchSuggestions(
            all,
            "chat.prompt",
            'event.payload.channel == "slack"',
        );
        expect(empty.map((s) => s.display_name)).toContain("universal");
        expect(narrowed.map((s) => s.display_name)).toContain("universal");
    });
});
