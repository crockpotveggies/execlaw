// Side-panel form behaviour: kind-specific fields, rename validation,
// AskAgent exit-tools editor.

import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { AutomationNodePanel } from "../settings/AutomationNodePanel";
import type { AutomationDef, NodeDef } from "../api/automations";

const baseDef: AutomationDef = {
    trigger: { kind: "webhook.received", when: null },
    nodes: [
        { id: "f1", kind: "Filter", config: { expr: "true" } },
        { id: "end", kind: "Terminal", config: {} },
    ],
    edges: [
        { from: "trigger", to: "f1", when: null },
        { from: "f1", to: "end", when: null },
    ],
};

describe("AutomationNodePanel — Filter form", () => {
    it("renders the Rhai expression textarea and propagates edits", () => {
        const node: NodeDef = baseDef.nodes[0];
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={node}
                definition={baseDef}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const ta = screen.getByTestId("node-panel-filter-expr") as HTMLTextAreaElement;
        expect(ta.value).toBe("true");
        fireEvent.change(ta, {
            target: { value: 'event.payload.zone == "driveway"' },
        });
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                id: "f1",
                kind: "Filter",
                config: { expr: 'event.payload.zone == "driveway"' },
            }),
        );
    });
});

describe("AutomationNodePanel — Transform form", () => {
    it("renders Transform expression and propagates edits", () => {
        const node: NodeDef = {
            id: "t1",
            kind: "Transform",
            config: { expr: "#{}" },
        };
        const def: AutomationDef = {
            ...baseDef,
            nodes: [...baseDef.nodes, node],
        };
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={node}
                definition={def}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const ta = screen.getByTestId("node-panel-transform-expr") as HTMLTextAreaElement;
        fireEvent.change(ta, { target: { value: "#{ doubled: 1 }" } });
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                config: { expr: "#{ doubled: 1 }" },
            }),
        );
    });
});

describe("AutomationNodePanel — rename input", () => {
    it("commits on Enter and calls onRename with the new id", () => {
        const onRename = vi.fn();
        render(
            <AutomationNodePanel
                node={baseDef.nodes[0]}
                definition={baseDef}
                onChange={vi.fn()}
                onRename={onRename}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const input = screen.getByTestId("node-panel-id-input") as HTMLInputElement;
        fireEvent.change(input, { target: { value: "filter_main" } });
        fireEvent.keyDown(input, { key: "Enter" });
        expect(onRename).toHaveBeenCalledWith("f1", "filter_main");
    });

    it("rejects duplicate ids without calling onRename", () => {
        const onRename = vi.fn();
        render(
            <AutomationNodePanel
                node={baseDef.nodes[0]}
                definition={baseDef}
                onChange={vi.fn()}
                onRename={onRename}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const input = screen.getByTestId("node-panel-id-input") as HTMLInputElement;
        fireEvent.change(input, { target: { value: "end" } });
        fireEvent.keyDown(input, { key: "Enter" });
        expect(onRename).not.toHaveBeenCalled();
        expect(screen.getByText(/already uses id/i)).toBeInTheDocument();
    });

    it("rejects reserved 'trigger' id", () => {
        const onRename = vi.fn();
        render(
            <AutomationNodePanel
                node={baseDef.nodes[0]}
                definition={baseDef}
                onChange={vi.fn()}
                onRename={onRename}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const input = screen.getByTestId("node-panel-id-input") as HTMLInputElement;
        fireEvent.change(input, { target: { value: "trigger" } });
        fireEvent.keyDown(input, { key: "Enter" });
        expect(onRename).not.toHaveBeenCalled();
        expect(screen.getByText(/reserved/i)).toBeInTheDocument();
    });
});

describe("AutomationNodePanel — AskAgent form", () => {
    it("warns when exit_tools is empty", () => {
        const node: NodeDef = {
            id: "a1",
            kind: "AskAgent",
            config: {
                prompt: "Decide.",
                attachments: [],
                exit_tools: [],
            },
        };
        render(
            <AutomationNodePanel
                node={node}
                definition={baseDef}
                onChange={vi.fn()}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        expect(screen.getByTestId("askagent-no-exit-tools")).toBeInTheDocument();
    });

    it("adds an exit tool when 'Add exit tool' is clicked", () => {
        const node: NodeDef = {
            id: "a1",
            kind: "AskAgent",
            config: {
                prompt: "Decide.",
                attachments: [],
                exit_tools: [],
            },
        };
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={node}
                definition={baseDef}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        fireEvent.click(screen.getByTestId("node-panel-askagent-add-tool"));
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                config: expect.objectContaining({
                    exit_tools: [
                        expect.objectContaining({
                            name: "tool_1",
                        }),
                    ],
                }),
            }),
        );
    });
});

describe("AutomationNodePanel — delete + close", () => {
    it("calls onDelete with the node id when the delete button is clicked", () => {
        const onDelete = vi.fn();
        render(
            <AutomationNodePanel
                node={baseDef.nodes[0]}
                definition={baseDef}
                onChange={vi.fn()}
                onRename={vi.fn()}
                onDelete={onDelete}
                onClose={vi.fn()}
            />,
        );
        fireEvent.click(screen.getByTestId("node-panel-delete"));
        expect(onDelete).toHaveBeenCalledWith("f1");
    });

    it("calls onClose when the close button is clicked", () => {
        const onClose = vi.fn();
        render(
            <AutomationNodePanel
                node={baseDef.nodes[0]}
                definition={baseDef}
                onChange={vi.fn()}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={onClose}
            />,
        );
        fireEvent.click(screen.getByTestId("node-panel-close"));
        expect(onClose).toHaveBeenCalledTimes(1);
    });
});
