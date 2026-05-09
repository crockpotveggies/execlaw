//! Streaming `<think>...</think>` stripper.
//!
//! 2026-04-28 — Qwen3.5's chat template can emit reasoning content
//! wrapped in `<think>...</think>` tags ahead of the visible reply.
//! We pass `chat_template_kwargs.enable_thinking=false` to suppress
//! this on the model side, but the operator-facing transcript should
//! still degrade gracefully if the template ignores the knob, the
//! model leaks a stray block, or a future model variant uses a
//! similar tag style.
//!
//! The filter is a tiny token-level state machine that runs over the
//! SSE delta stream:
//!
//!   * **Outside** a think block — emit text verbatim, watching for
//!     `<think>` (case-insensitive). If the buffer ends in a partial
//!     prefix of `<think>`, hold those bytes back so we don't ship a
//!     `<` that turns into a tag on the next chunk.
//!   * **Inside** a think block — drop everything until `</think>`,
//!     then resume Outside on the byte after the closing tag.
//!
//! Edge cases handled:
//!   * Tags split across chunks (`"...<th"` then `"ink>..."`).
//!   * Multiple think blocks in one stream (unusual, but cheap to
//!     support).
//!   * Mixed-case tags (`<Think>`, `</THINK>`).
//!   * Stream ends mid-tag — we conservatively flush whatever pending
//!     bytes we held back so the operator at least sees something.

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Outside,
    Inside,
}

#[derive(Debug, Clone)]
pub struct ThinkBlockFilter {
    state: State,
    /// Buffer of characters that *might* be the start of a tag.
    /// Held back from emission until we see enough of the next chunk
    /// to decide. Always shorter than the longest tag length.
    pending: String,
}

impl ThinkBlockFilter {
    pub fn new() -> Self {
        Self {
            state: State::Outside,
            pending: String::new(),
        }
    }

    /// Consume `chunk` and return the visible bytes (with `<think>`
    /// blocks stripped). May return an empty string while waiting on
    /// more bytes of an in-progress tag.
    pub fn feed(&mut self, chunk: &str) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(chunk);
        let mut out = String::new();
        loop {
            match self.state {
                State::Outside => {
                    let lower = buf.to_ascii_lowercase();
                    if let Some(idx) = lower.find(OPEN_TAG) {
                        // Emit everything before the tag, then enter
                        // the Inside state and drop the tag itself.
                        out.push_str(&buf[..idx]);
                        let after = idx + OPEN_TAG.len();
                        buf.drain(..after);
                        self.state = State::Inside;
                        continue;
                    }
                    // No complete `<think>` in `buf`. Emit everything
                    // EXCEPT a trailing prefix that might still grow
                    // into the tag on the next chunk.
                    let safe_len = trailing_safe_len(&buf, OPEN_TAG);
                    out.push_str(&buf[..safe_len]);
                    self.pending = buf[safe_len..].to_owned();
                    return out;
                }
                State::Inside => {
                    let lower = buf.to_ascii_lowercase();
                    if let Some(idx) = lower.find(CLOSE_TAG) {
                        // Drop everything up to and including the
                        // closing tag, then resume Outside on the
                        // remainder.
                        let after = idx + CLOSE_TAG.len();
                        buf.drain(..after);
                        self.state = State::Outside;
                        continue;
                    }
                    // Still inside. Drop everything except a trailing
                    // prefix that might still grow into the closing
                    // tag.
                    let safe_len = trailing_safe_len(&buf, CLOSE_TAG);
                    // Discard the safely-confirmed reasoning bytes;
                    // hold back the rest in case `</think>` straddles
                    // the chunk boundary.
                    self.pending = buf[safe_len..].to_owned();
                    return out;
                }
            }
        }
    }

    /// Stream is closing. Whatever was in `pending` is no longer
    /// going to grow — emit it as-is so a stray `<` at the very end
    /// of the response doesn't get silently swallowed.
    #[allow(dead_code)] // wired into the chat handler when we plumb a flush call
    pub fn flush(&mut self) -> String {
        let tail = std::mem::take(&mut self.pending);
        if self.state == State::Inside {
            // We never saw the closing tag — assume the operator
            // doesn't want to see the unterminated reasoning.
            String::new()
        } else {
            tail
        }
    }
}

impl Default for ThinkBlockFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Largest byte-prefix of `buf` that is GUARANTEED not to be a prefix
/// of `tag` once more bytes arrive. In practice: cut off any trailing
/// suffix of `buf` that matches a prefix of `tag` (case-insensitive),
/// because that suffix could grow into the tag once we see the next
/// chunk.
fn trailing_safe_len(buf: &str, tag: &str) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let tag_lower = tag.to_ascii_lowercase();
    let buf_lower = buf.to_ascii_lowercase();
    // Try the longest possible suffix of buf that's a prefix of tag.
    // We can't hold back more than tag.len()-1 bytes (any longer and
    // a complete tag would already be present, which the caller
    // checks first).
    let max_check = (tag.len() - 1).min(buf.len());
    for n in (1..=max_check).rev() {
        if buf.is_char_boundary(buf.len() - n) {
            let suffix = &buf_lower[buf.len() - n..];
            if tag_lower.starts_with(suffix) {
                return buf.len() - n;
            }
        }
    }
    buf.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_no_think_tag() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("hello world"), "hello world");
        assert_eq!(f.flush(), "");
    }

    #[test]
    fn strips_complete_think_block_in_one_chunk() {
        let mut f = ThinkBlockFilter::new();
        let out = f.feed("Hi <think>let me reason</think> world");
        assert_eq!(out, "Hi  world");
    }

    #[test]
    fn strips_think_block_split_across_chunks() {
        let mut f = ThinkBlockFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("Hi <thi"));
        out.push_str(&f.feed("nk>secret"));
        out.push_str(&f.feed(" reasoning</thi"));
        out.push_str(&f.feed("nk> visible"));
        out.push_str(&f.flush());
        assert_eq!(out, "Hi  visible");
    }

    #[test]
    fn strips_when_chunk_starts_inside_block() {
        // Content begins with the entire reasoning block — the
        // operator should see nothing until after the close tag.
        let mut f = ThinkBlockFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("<think>"));
        assert_eq!(out, "");
        out.push_str(&f.feed("planning..."));
        assert_eq!(out, "");
        out.push_str(&f.feed("</think>Hello."));
        out.push_str(&f.flush());
        assert_eq!(out, "Hello.");
    }

    #[test]
    fn handles_multiple_think_blocks() {
        let mut f = ThinkBlockFilter::new();
        let out = f.feed("a<think>x</think>b<think>y</think>c");
        assert_eq!(out, "abc");
    }

    #[test]
    fn case_insensitive_tags() {
        let mut f = ThinkBlockFilter::new();
        let out = f.feed("a<Think>x</THINK>b");
        assert_eq!(out, "ab");
    }

    #[test]
    fn lonely_open_tag_at_eof_drops_trailing_buffer() {
        // Stream ends with an open tag and no close — the operator
        // shouldn't see partial reasoning content.
        let mut f = ThinkBlockFilter::new();
        let out = f.feed("ok<think>reasoning leaked");
        assert_eq!(out, "ok");
        let tail = f.flush();
        assert_eq!(tail, "", "unterminated <think> body must not flush");
    }

    #[test]
    fn trailing_partial_tag_is_held_until_resolved() {
        // The chunk ends with `<` which COULD be `<think>` or could
        // just be text. We must NOT emit it yet.
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("hi<"), "hi");
        // Resolve as plain text on the next chunk.
        assert_eq!(f.feed("3 cool"), "<3 cool");
    }

    #[test]
    fn trailing_partial_tag_resolves_into_real_tag() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("hi<"), "hi");
        assert_eq!(f.feed("think>secret</think> done"), " done");
    }

    #[test]
    fn flush_returns_held_pending_outside_block() {
        // Pending text that's not part of any tag must be emitted
        // when the stream closes.
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("hi<"), "hi");
        assert_eq!(f.flush(), "<");
    }
}
