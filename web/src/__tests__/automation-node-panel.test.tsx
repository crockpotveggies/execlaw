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

describe("AutomationNodePanel — Notify form", () => {
    const notifyNode: NodeDef = {
        id: "alert1",
        kind: "Notify",
        config: {
            title: "Motion in {{event.payload.zone}}",
            detail: "",
            severity: "Warning",
        },
    };

    it("renders title + severity dropdown + propagates edits", () => {
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={notifyNode}
                definition={baseDef}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const title = screen.getByTestId("node-panel-notify-title") as HTMLInputElement;
        expect(title.value).toBe("Motion in {{event.payload.zone}}");
        fireEvent.change(title, { target: { value: "New title" } });
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                config: expect.objectContaining({ title: "New title" }),
            }),
        );

        const sev = screen.getByTestId("node-panel-notify-severity") as HTMLSelectElement;
        expect(sev.value).toBe("Warning");
        fireEvent.change(sev, { target: { value: "Critical" } });
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                config: expect.objectContaining({ severity: "Critical" }),
            }),
        );
    });
});

describe("AutomationNodePanel — CallPlugin form", () => {
    const cpNode: NodeDef = {
        id: "call1",
        kind: "CallPlugin",
        config: {
            tool: "signal.send_message",
            args: { to: "+15551234", body: "hi" },
        },
    };

    it("renders tool input + JSON args textarea", () => {
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={cpNode}
                definition={baseDef}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const tool = screen.getByTestId("node-panel-callplugin-tool") as HTMLInputElement;
        expect(tool.value).toBe("signal.send_message");
        fireEvent.change(tool, { target: { value: "whatsapp.send_message" } });
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                config: expect.objectContaining({ tool: "whatsapp.send_message" }),
            }),
        );

        const args = screen.getByTestId("node-panel-callplugin-args") as HTMLTextAreaElement;
        expect(args.value).toContain("+15551234");
    });

    it("commits args on blur only when the JSON parses to an object", () => {
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={cpNode}
                definition={baseDef}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const args = screen.getByTestId("node-panel-callplugin-args") as HTMLTextAreaElement;
        // Bad JSON: nothing committed, error visible.
        fireEvent.change(args, { target: { value: "{ not json" } });
        fireEvent.blur(args);
        expect(onChange).not.toHaveBeenCalled();
        expect(
            screen.getByTestId("node-panel-callplugin-args-error"),
        ).toBeInTheDocument();

        // Good JSON: committed via onChange.
        fireEvent.change(args, { target: { value: '{"x": 1}' } });
        fireEvent.blur(args);
        expect(onChange).toHaveBeenCalledWith(
            expect.objectContaining({
                config: expect.objectContaining({ args: { x: 1 } }),
            }),
        );
    });

    it("rejects array args (must be an object, not a list)", () => {
        const onChange = vi.fn();
        render(
            <AutomationNodePanel
                node={cpNode}
                definition={baseDef}
                onChange={onChange}
                onRename={vi.fn()}
                onDelete={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const args = screen.getByTestId("node-panel-callplugin-args") as HTMLTextAreaElement;
        fireEvent.change(args, { target: { value: "[1,2,3]" } });
        fireEvent.blur(args);
        expect(onChange).not.toHaveBeenCalled();
        expect(
            screen.getByTestId("node-panel-callplugin-args-error"),
        ).toBeInTheDocument();
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
