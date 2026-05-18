//! execlaw-shaped MIME bundles + `python.execute` result envelope.
//!
//! Jupyter's iopub messages carry "MIME bundles" — a map of
//! `mime_type -> data` where `data` can be a string, bytes (base64),
//! or arbitrary JSON depending on the type. We re-shape this into a
//! list-of-pairs that's:
//!
//! 1. Friendlier to JSON-Schema validation (a map with arbitrary keys
//!    is harder to validate than a tagged-union list).
//! 2. Stable in ordering — when an iopub message carries both
//!    `text/plain` and `text/html` (DataFrames do this; verified in
//!    Phase 1 smoke), the SPA's chip renderer wants a predictable
//!    order to pick the richest representation.
//! 3. Easy to attach binary outputs (PNG, parquet preview, etc.) as
//!    `attachment_id` refs instead of inlining base64 in a tool result.
//!
//! The shape declared here is the over-the-wire JSON that lands in
//! the agent's `tool_result` content block AND in the SPA's
//! `MessageStream` dispatcher. Any change here is a coordinated
//! breaking change across Rust, the Jupyter parser, and the SPA's
//! chip renderer (Phase 10).

use serde::{Deserialize, Serialize};

/// One MIME-typed slice of an output. Sibling slices in a single
/// [`ExecuteOutput::ExecuteResult`] / `DisplayData` represent the SAME
/// logical value rendered different ways (e.g. a DataFrame as both
/// `text/plain` and `text/html`); the SPA picks the richest one its
/// chip renderer supports.
///
/// `data` is left as `serde_json::Value` so callers don't have to
/// commit to text-vs-bytes encoding here; the renderer interprets
/// based on `mime_type`. Conventions:
///   - `text/plain`, `text/html`, `application/json`, `text/markdown`
///     → string (UTF-8).
///   - `image/png`, `image/jpeg`, `image/webp` → string (base64,
///     unpadded `=` allowed; the Jupyter gateway sends padded).
///   - `application/vnd.plotly.v1+json` and other `+json` types →
///     JSON value (object/array/etc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MimeBundle {
    pub mime_type: String,
    pub data: serde_json::Value,
}

/// Name of a stdout/stderr stream. Kept as an enum rather than a free
/// string so the SPA renderer can style stderr in red without
/// pattern-matching against magic strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamName {
    Stdout,
    Stderr,
}

/// One output emitted during a single `execute_request`. Mirrors the
/// distinct iopub message types we care about, NOT a faithful 1:1
/// mapping — `execute_input` and `status` are bookkeeping that's
/// consumed internally and not surfaced as outputs.
///
/// Discriminator field `kind` so the SPA can route to its per-output
/// renderer (code pill expands the `bundle` for `execute_result`,
/// chip strip for `display_data`, monospace block for `stream`,
/// red-traceback panel for `error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecuteOutput {
    // (variants below)
    /// stdout/stderr text chunk. Multiple `stream` outputs in a row
    /// are NOT pre-concatenated by us — the SPA collapses adjacent
    /// chunks with the same `name` for display, but the wire-level
    /// list preserves chunk boundaries so timing-sensitive callers
    /// (logs, progress bars) keep their structure.
    Stream { name: StreamName, text: String },

    /// The cell's "return value" — e.g. the last expression in a
    /// notebook cell. Carries a MIME bundle so DataFrames come
    /// through as both `text/html` AND `text/plain`.
    ExecuteResult {
        execution_count: u32,
        bundle: Vec<MimeBundle>,
    },

    /// Output emitted via `display(...)` or any `_repr_*_` hook
    /// during the execute (not the cell's return value).
    DisplayData { bundle: Vec<MimeBundle> },

    /// An uncaught Python exception. `traceback` retains the ANSI
    /// colorization that IPython generates — the SPA strips ANSI on
    /// render (or keeps it, behind a "raw" toggle).
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

impl ExecuteOutput {
    /// Approximate on-wire byte size of this output. Used by the
    /// execute loop's running-total guard to decide when to trip
    /// [`ExecuteStatus::OutputTooLarge`].
    ///
    /// "Approximate" because we don't serialize the full JSON to
    /// measure — for string-typed MIME bundle data we use the
    /// string's UTF-8 length directly; for non-string JSON values
    /// we serialize once. The cap is a coarse guardrail (50 MB),
    /// not a billing-grade meter, so the approximation is fine.
    pub fn approx_bytes(&self) -> usize {
        match self {
            ExecuteOutput::Stream { text, .. } => text.len(),
            ExecuteOutput::ExecuteResult { bundle, .. } | ExecuteOutput::DisplayData { bundle } => {
                bundle.iter().map(MimeBundle::approx_bytes).sum()
            }
            ExecuteOutput::Error {
                ename,
                evalue,
                traceback,
            } => {
                ename.len()
                    + evalue.len()
                    + traceback.iter().map(String::len).sum::<usize>()
            }
        }
    }
}

impl MimeBundle {
    /// Approximate on-wire byte size. See [`ExecuteOutput::approx_bytes`].
    pub fn approx_bytes(&self) -> usize {
        let data_size = match &self.data {
            serde_json::Value::String(s) => s.len(),
            other => serde_json::to_string(other).map(|s| s.len()).unwrap_or(0),
        };
        self.mime_type.len() + data_size
    }
}

/// Terminal status of an `execute_request`. The agent reads this to
/// decide whether to autoretry (per the UX plan, cap = 2 attempts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteStatus {
    /// `execute_reply.status == "ok"`.
    Ok,
    /// `execute_reply.status == "error"`. There will be a matching
    /// [`ExecuteOutput::Error`] in `outputs`.
    Error,
    /// Wall-clock deadline tripped; the kernel was sent an interrupt
    /// signal. Partial `outputs` may still be populated with whatever
    /// arrived before the deadline.
    Timeout,
    /// Kernel exited mid-execute (crashed, was killed by the OOM
    /// killer, etc.). No further outputs will arrive on this
    /// execution_count.
    KernelDied,
    /// Accumulated output crossed the byte ceiling
    /// ([`crate::python_sandbox::client::MAX_OUTPUT_BYTES`]); the
    /// kernel was interrupted. A synthetic
    /// [`ExecuteOutput::Error`] with ename=`OutputTooLarge` is the
    /// last element of `outputs`. Defense against an adversarial
    /// cell that prints 1 GB of stdout and OOMs the host.
    OutputTooLarge,
}

/// What `python.execute` returns to the tool dispatcher.
///
/// Wire-compatible with the tool description in `plugin.toml` — the
/// agent's mental model and our types agree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecuteResult {
    pub outputs: Vec<ExecuteOutput>,
    pub execution_count: u32,
    pub duration_ms: u64,
    pub status: ExecuteStatus,

    /// Files the supervisor's `/work/<convo>/outputs/` watcher
    /// observed during this execute (Phase 4). Populated by the
    /// tool dispatcher AFTER the WS execute resolves; this module
    /// leaves it `None` so a unit test of the wire layer doesn't
    /// have to mock the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_files: Option<Vec<CreatedFileRef>>,
}

/// Pointer to a file the agent's execute produced. Just enough for
/// the SPA's chip renderer to render the chip AND the agent's NEXT
/// turn to refer to it by name. The actual blob is in the artifact
/// store; `attachment_id` is the lookup key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedFileRef {
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub attachment_id: String,
}

/// Convert Jupyter's `data: { "text/plain": "...", "text/html": "..." }`
/// map into our list-of-pairs MIME bundle.
///
/// Stable order: MIME types are sorted lexicographically. The SPA's
/// chip renderer picks by its own priority list (richest supported
/// MIME wins) so ordering doesn't affect rendering — but sorting
/// makes tool_result diffs deterministic between replays of the
/// same conversation, which the agent's caching layer relies on.
pub fn mime_bundle_from_jupyter_data(
    data: &serde_json::Map<String, serde_json::Value>,
) -> Vec<MimeBundle> {
    let mut out: Vec<MimeBundle> = data
        .iter()
        .map(|(mime_type, data)| MimeBundle {
            mime_type: mime_type.clone(),
            data: data.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.mime_type.cmp(&b.mime_type));
    out
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn execute_output_stream_round_trips() {
        let out = ExecuteOutput::Stream {
            name: StreamName::Stderr,
            text: "uh oh\n".into(),
        };
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(
            json,
            json!({"kind": "stream", "name": "stderr", "text": "uh oh\n"})
        );
        let back: ExecuteOutput = serde_json::from_value(json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn execute_output_result_round_trips_rich_mime() {
        // Mirrors what Phase 1 smoke showed: a pandas DataFrame's
        // execute_result yields BOTH text/plain and text/html. The
        // SPA needs both intact and in stable order.
        let out = ExecuteOutput::ExecuteResult {
            execution_count: 2,
            bundle: vec![
                MimeBundle {
                    mime_type: "text/plain".into(),
                    data: json!("   a  b\n0  1  3\n1  2  4"),
                },
                MimeBundle {
                    mime_type: "text/html".into(),
                    data: json!("<table>...</table>"),
                },
            ],
        };
        let json = serde_json::to_value(&out).unwrap();
        let back: ExecuteOutput = serde_json::from_value(json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn execute_output_error_carries_full_traceback() {
        let out = ExecuteOutput::Error {
            ename: "ZeroDivisionError".into(),
            evalue: "division by zero".into(),
            // The ANSI escapes are preserved on the wire — Phase 1
            // smoke confirmed the gateway sends them like this.
            traceback: vec![
                "\u{1b}[31m---\u{1b}[0m".into(),
                "ZeroDivisionError: division by zero".into(),
            ],
        };
        let json = serde_json::to_value(&out).unwrap();
        let back: ExecuteOutput = serde_json::from_value(json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn execute_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(ExecuteStatus::Ok).unwrap(),
            json!("ok")
        );
        assert_eq!(
            serde_json::to_value(ExecuteStatus::Timeout).unwrap(),
            json!("timeout")
        );
        assert_eq!(
            serde_json::to_value(ExecuteStatus::KernelDied).unwrap(),
            json!("kernel_died")
        );
        assert_eq!(
            serde_json::to_value(ExecuteStatus::OutputTooLarge).unwrap(),
            json!("output_too_large")
        );
    }

    #[test]
    fn approx_bytes_counts_stream_text() {
        let o = ExecuteOutput::Stream {
            name: StreamName::Stdout,
            text: "hello\n".into(),
        };
        assert_eq!(o.approx_bytes(), 6);
    }

    #[test]
    fn approx_bytes_counts_mime_bundle_strings_directly() {
        // For string-typed MIME data we use UTF-8 byte length
        // directly — much cheaper than full JSON re-serialize and
        // good enough for a 50 MB guardrail.
        let o = ExecuteOutput::ExecuteResult {
            execution_count: 1,
            bundle: vec![
                MimeBundle {
                    mime_type: "text/plain".into(),
                    data: json!("hi"),
                },
                MimeBundle {
                    mime_type: "text/html".into(),
                    data: json!("<b>hi</b>"),
                },
            ],
        };
        // text/plain (10) + "hi" (2) + text/html (9) + "<b>hi</b>" (9) = 30
        assert_eq!(o.approx_bytes(), 30);
    }

    #[test]
    fn approx_bytes_handles_non_string_mime_data() {
        // Plotly figures arrive as JSON objects. Fall back to
        // serialize-and-measure for those.
        let o = ExecuteOutput::DisplayData {
            bundle: vec![MimeBundle {
                mime_type: "application/json".into(),
                data: json!({"x": 1}),
            }],
        };
        let n = o.approx_bytes();
        // "application/json" (16) + r#"{"x":1}"# (7) = 23
        assert_eq!(n, 23);
    }

    #[test]
    fn approx_bytes_counts_error_fields() {
        let o = ExecuteOutput::Error {
            ename: "ValueError".into(),     // 10
            evalue: "bad input".into(),     // 9
            traceback: vec!["a".into(), "bc".into()], // 1 + 2
        };
        assert_eq!(o.approx_bytes(), 10 + 9 + 1 + 2);
    }

    #[test]
    fn mime_bundle_from_jupyter_data_sorts_stably() {
        // Real shape: pandas DataFrame execute_result carries both
        // text/html and text/plain. We sort lexicographically so a
        // re-execute produces byte-identical tool_results.
        let mut data = serde_json::Map::new();
        data.insert("text/plain".into(), json!("   a  b\n0  1  3"));
        data.insert(
            "text/html".into(),
            json!("<table>...</table>"),
        );
        let bundle = mime_bundle_from_jupyter_data(&data);
        assert_eq!(bundle.len(), 2);
        assert_eq!(bundle[0].mime_type, "text/html");
        assert_eq!(bundle[1].mime_type, "text/plain");
    }

    #[test]
    fn mime_bundle_from_jupyter_data_empty_yields_empty() {
        let bundle = mime_bundle_from_jupyter_data(&serde_json::Map::new());
        assert!(bundle.is_empty());
    }

    #[test]
    fn execute_result_skips_created_files_when_none() {
        // Tool dispatcher populates this AFTER the supervisor's
        // file watcher reports outputs. Wire layer must serialize
        // it as ABSENT (not `null`) when None so the schema's
        // `additionalProperties: false` plays nicely.
        let r = ExecuteResult {
            outputs: vec![],
            execution_count: 1,
            duration_ms: 12,
            status: ExecuteStatus::Ok,
            created_files: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            !s.contains("created_files"),
            "created_files must be omitted when None; got {s}"
        );
    }
}
