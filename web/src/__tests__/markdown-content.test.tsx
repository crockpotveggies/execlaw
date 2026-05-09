// Tests for MarkdownContent — covers both the static-render path
// (committed agent / user messages) and the streaming-aware split
// that gates rendering until each line lands.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { MarkdownContent } from "../components/MarkdownContent";

describe("MarkdownContent (static)", () => {
    it("renders **bold** as <strong>", () => {
        const { container } = render(
            <MarkdownContent text="hello **world**" />,
        );
        expect(container.querySelector("strong")?.textContent).toBe("world");
    });

    it("renders inline `code` as <code> outside of <pre>", () => {
        const { container } = render(
            <MarkdownContent text="run `npm test` to verify" />,
        );
        const code = container.querySelector("code");
        expect(code?.textContent).toBe("npm test");
        // Inline code is NOT wrapped in <pre>.
        expect(code?.parentElement?.tagName.toLowerCase()).not.toBe("pre");
    });

    it("renders fenced ``` blocks as <pre><code>", () => {
        const text = "```\nconsole.log('hi');\n```";
        const { container } = render(<MarkdownContent text={text} />);
        const pre = container.querySelector("pre");
        const code = pre?.querySelector("code");
        expect(pre).toBeTruthy();
        expect(code?.textContent).toBe("console.log('hi');\n");
    });

    /// User-explicit contract (2026-04-28): markdown INSIDE a code
    /// block must be preserved as literal text. `# heading` between
    /// fences must NOT render as an <h1>.
    it("does NOT render markdown inside a fenced code block", () => {
        const text = "```\n# not a heading\n**not bold**\n```";
        const { container } = render(<MarkdownContent text={text} />);
        // No <h1> / <strong> children — they live as literal text
        // inside the <code> element.
        expect(container.querySelector("h1")).toBeNull();
        expect(container.querySelector("strong")).toBeNull();
        const codeText = container.querySelector("pre code")?.textContent ?? "";
        expect(codeText).toContain("# not a heading");
        expect(codeText).toContain("**not bold**");
    });

    it("renders unordered lists as <ul><li>", () => {
        const text = "- one\n- two\n- three";
        const { container } = render(<MarkdownContent text={text} />);
        expect(container.querySelectorAll("ul li")).toHaveLength(3);
    });

    it("renders headings as <h1>/<h2>/<h3>", () => {
        const { container } = render(
            <MarkdownContent text={"# H1\n## H2\n### H3"} />,
        );
        expect(container.querySelector("h1")?.textContent).toBe("H1");
        expect(container.querySelector("h2")?.textContent).toBe("H2");
        expect(container.querySelector("h3")?.textContent).toBe("H3");
    });

    it("renders links with target=_blank rel=noopener", () => {
        const { container } = render(
            <MarkdownContent text="[execlaw](https://example.com)" />,
        );
        const a = container.querySelector("a");
        expect(a?.getAttribute("href")).toBe("https://example.com");
        expect(a?.getAttribute("target")).toBe("_blank");
        // jsdom may return the rel as "noopener noreferrer" or split.
        expect(a?.getAttribute("rel") ?? "").toContain("noopener");
        expect(a?.getAttribute("rel") ?? "").toContain("noreferrer");
    });

    it("renders gfm tables", () => {
        const text =
            "| col | val |\n| --- | --- |\n| a | 1 |\n| b | 2 |";
        const { container } = render(<MarkdownContent text={text} />);
        expect(container.querySelector("table")).toBeTruthy();
        expect(container.querySelectorAll("tbody tr")).toHaveLength(2);
    });

    it("converts single newlines to <br> via remark-breaks", () => {
        const { container } = render(
            <MarkdownContent text={"line one\nline two"} />,
        );
        // A single Enter in a chat message renders as a <br>, not
        // collapsed to a space — matches the standard chat-app
        // expectation.
        expect(container.querySelector("br")).toBeTruthy();
    });

    it("escapes raw HTML — <script> renders as text, not as a node", () => {
        const { container } = render(
            <MarkdownContent text="<script>alert('xss')</script>" />,
        );
        expect(container.querySelector("script")).toBeNull();
        expect(container.textContent).toContain(
            "<script>alert('xss')</script>",
        );
    });
});

describe("MarkdownContent (streaming)", () => {
    /// The streaming gate splits at the LAST `\n`. Lines before
    /// that are committed (rendered as markdown); the trailing
    /// fragment renders as plain text so a half-formed `**` or
    /// `[link]` doesn't flicker into bold-then-unbold.
    it("renders committed lines as markdown and keeps the trailing fragment plain", () => {
        // Last \n at index 14 → committed = "**bold one**\n",
        // trailing = "**bold tw" (mid-stream — must NOT bold).
        const { container } = render(
            <MarkdownContent
                text={"**bold one**\n**bold tw"}
                streaming
            />,
        );
        // Exactly one <strong> for the committed line.
        const strongs = container.querySelectorAll("strong");
        expect(strongs).toHaveLength(1);
        expect(strongs[0].textContent).toBe("bold one");
        // The trailing partial markdown is preserved as raw text.
        const trailing = container.querySelector(
            '[data-testid="md-trailing-plain"]',
        );
        expect(trailing?.textContent).toBe("**bold tw");
    });

    it("a stream with no completed line yet renders entirely as plain text", () => {
        const { container } = render(
            <MarkdownContent
                text={"**still typing"}
                streaming
            />,
        );
        // Nothing committed → no <strong> renderings even though
        // the text contains **.
        expect(container.querySelector("strong")).toBeNull();
        const trailing = container.querySelector(
            '[data-testid="md-trailing-plain"]',
        );
        expect(trailing?.textContent).toBe("**still typing");
    });

    /// Open code fences mid-stream are intentionally rendered (the
    /// fence opener is on a committed line). This is the "graceful
    /// open-to-EOF" behaviour of remark — the user sees a code
    /// block growing as the body lines stream in.
    it("an open ```fence on a committed line renders as a code block", () => {
        const text = "```\nprint(";
        const { container } = render(
            <MarkdownContent text={text} streaming />,
        );
        // The `print(` is on the trailing fragment, but the open
        // fence is on the committed line so the rendered DOM
        // includes a <pre> from the partial committed prefix.
        // (The trailing fragment falls outside as plain text —
        // it'll fold INTO the code block once its closing newline
        // lands and on the next streaming chunk.)
        expect(container.querySelector("pre")).toBeTruthy();
    });
});
