// Markdown renderer for chat message bodies.
//
// Applied to both agent and user messages — same as every consumer
// chat UI ships. GFM is enabled (tables, strikethrough, autolinks,
// task-lists). Raw HTML is NOT rendered — react-markdown's default
// is to escape it, and we don't override, so an agent that emits
// `<script>` literally renders the angle-brackets.
//
// Streaming-aware split (2026-04-28): when `streaming` is true, the
// component splits the input at the last `\n` and renders only the
// "committed" prefix as markdown — the in-progress trailing line
// stays as raw text until its own newline lands. This avoids the
// flicker of `**` / `[` / `# ` etc. half-rendering as they stream
// in token-by-token. Side benefits:
//
//   * An unterminated fenced code block (` ```py\n`) renders
//     correctly because remark treats unterminated fences as
//     open-to-EOF — and the open fence sits in the committed
//     prefix while the body lines stream in.
//   * Markdown INSIDE a fenced code block is NEVER rendered as
//     markdown (this is remark's default behaviour for code blocks
//     and is what the user explicitly asked for).
//
// The split-at-last-newline approach is intentionally simple. We
// don't try to detect "you're in the middle of a backtick pair" or
// similar — once the streaming chunk finishes a line, it gets
// rendered. This matches the natural cadence of LLM output.

import ReactMarkdown from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

// remark-breaks: render every literal `\n` inside a paragraph as a
// `<br>` instead of collapsing it to a space. Matches the chat-app
// convention where a single Enter produces a line break, not a
// re-flowed paragraph. (Two consecutive `\n` still mean "new
// paragraph" via standard markdown semantics.)
const REMARK_PLUGINS = [remarkGfm, remarkBreaks];

interface Props {
    text: string;
    /** When true, gate markdown rendering on whole-line completion
     *  so partial markdown (`**`, half-`[link]`, mid-fence) doesn't
     *  flicker as tokens stream in. Defaults to false. */
    streaming?: boolean;
}

export function MarkdownContent({ text, streaming = false }: Props) {
    if (!streaming) {
        return (
            <ReactMarkdown
                remarkPlugins={REMARK_PLUGINS}
                // Add the standard execlaw class so theme.scss can
                // target ".execlaw-md a", ".execlaw-md pre" etc. without
                // colliding with bootstrap's element selectors.
                components={WRAPPED}
            >
                {text}
            </ReactMarkdown>
        );
    }

    // Split at the LAST newline. Everything before is "committed" —
    // it's been on screen long enough that line-level markdown is
    // safe to apply. The trailing fragment is mid-stream; render
    // it as plain text so a partial `**foo` doesn't flash bold-then-
    // un-bold as the closing `**` arrives.
    const idx = text.lastIndexOf("\n");
    const committed = idx >= 0 ? text.slice(0, idx + 1) : "";
    const trailing = idx >= 0 ? text.slice(idx + 1) : text;

    return (
        <>
            {committed.length > 0 && (
                <ReactMarkdown
                    remarkPlugins={REMARK_PLUGINS}
                    components={WRAPPED}
                >
                    {committed}
                </ReactMarkdown>
            )}
            {trailing.length > 0 && (
                <span data-testid="md-trailing-plain">{trailing}</span>
            )}
        </>
    );
}

// Override a couple of element renders so links open safely + code
// blocks gain a stable hook for theming.
const WRAPPED = {
    a: ({ href, children, ...rest }: { href?: string; children?: React.ReactNode }) => (
        <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            {...rest}
        >
            {children}
        </a>
    ),
};
