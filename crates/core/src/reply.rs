//! Reply payload model (M6 event-driven architecture).
//!
//! `ReplyPayload` is what `SendReply` hands to the `ReplyRouter`.
//! It describes the *content* of an agent or tool response in a
//! way that's rich enough for the web UI yet structured enough that
//! per-transport degradation logic in the router can decide how to
//! pack each part for SMS, WhatsApp, voice TTS, etc.
//!
//! Design principles:
//!   * Reference existing attachment / artifact rows by id — never
//!     embed bytes in the payload. The `AttachmentStore` already
//!     handles content addressing and per-conversation scoping.
//!   * Every part is optional except `text`. A reply ALWAYS has
//!     some text; renderers that can't display rich parts use this
//!     as the fallback body. The router synthesizes one-line
//!     descriptions when text is empty + only rich parts present.
//!   * Streaming is a first-class variant of the parts list, not a
//!     bolted-on second mechanism. The same enum that describes
//!     static payloads describes streamed deltas.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What `SendReply` emits. The router degrades per-transport before
/// emission — see `crates/server/src/reply_router/degrade.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ReplyPayload {
    /// Primary text body. Always present — every transport can
    /// render some text. Empty string is legal (attachments-only
    /// reply); the router will synthesize a one-line summary in
    /// that case so SMS / voice don't get nothing.
    pub text: String,

    /// Ordered rich parts. Render order matters — renderers append
    /// in array order under (or after) the text body.
    #[serde(default)]
    pub parts: Vec<ReplyPart>,

    /// Per-reply hints. Author-controlled overrides of router
    /// defaults — most flows leave this at `Default::default()`.
    #[serde(default)]
    pub hints: ReplyHints,
}

/// One rich content unit in a reply.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyPart {
    /// Existing `AttachmentRow` (conversation-scoped — uploads,
    /// screenshots, voice clips). The router emits a signed download
    /// URL via the `download_urls` subsystem.
    Attachment {
        attachment_id: String,
        caption: Option<String>,
    },

    /// Existing `ArtifactRow` (plugin-emitted, possibly TTL'd —
    /// charts, generated PDFs, CSVs, python_sandbox /work outputs).
    Artifact {
        artifact_id: String,
        caption: Option<String>,
    },

    /// Inline chart spec. Renderers that can't draw rasterize via
    /// the chart subsystem; renderers that can render directly.
    Chart {
        /// Either a Vega-Lite spec object or `{ "kind": "svg",
        /// "data": "<svg xmlns…/>" }` for raw SVG. The router
        /// inspects `spec.kind` to pick the renderer.
        #[schema(value_type = Object)]
        spec: serde_json::Value,
        /// Theme override. `None` = inherit from operator preference.
        theme: Option<ChartTheme>,
        caption: Option<String>,
    },

    /// Tabular data. Rich transports render as a Card; semi-rich as
    /// truncated text rows; voice summarizes.
    Table {
        columns: Vec<String>,
        #[schema(value_type = Object)]
        rows: Vec<Vec<serde_json::Value>>,
        caption: Option<String>,
    },

    /// Key-value card. Renders as a structured card on web; flattens
    /// to "title\nfield: value\n…" on poor transports.
    Card {
        title: String,
        fields: Vec<CardField>,
    },

    /// External file by URL. Rich transports download + attach; poor
    /// transports forward the URL inline. The router caches to an
    /// `ArtifactRow` before forwarding to multiple transports so the
    /// origin URL isn't hit N times.
    ExternalFile {
        url: String,
        filename: String,
        mime_type: String,
        size_bytes: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct CardField {
    pub label: String,
    pub value: String,
    /// Optional accent — renders as a colored chip on rich
    /// transports (e.g., `"success"`, `"warning"`).
    pub kind: Option<String>,
}

/// Theme preset for inline chart rendering. Operator picks the
/// default in Settings; flows can override per-chart.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChartTheme {
    /// Default `execlaw_dark` preset shipped in
    /// `crates/core/src/chart_themes/execlaw_dark.json`.
    ExeclawDark,
    /// `execlaw_light` preset for printer-friendly or email output.
    ExeclawLight,
    /// Arbitrary Vega-Lite `config` block, merged on top of the
    /// chosen base (default base = ExeclawDark unless the spec
    /// already declares one).
    Custom {
        #[schema(value_type = Object)]
        config: serde_json::Value,
    },
}

impl ChartTheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExeclawDark => "execlaw_dark",
            Self::ExeclawLight => "execlaw_light",
            Self::Custom { .. } => "custom",
        }
    }
}

/// Per-reply behavioral hints to the router.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ReplyHints {
    /// "send everything in one message" vs "one message per part".
    /// WhatsApp / Signal often want per-attachment messages for
    /// readability; default is `false` (bundled).
    #[serde(default)]
    pub split_per_part: bool,

    /// Where to put the work product if delivery fails. Defaults to
    /// `ChatAppendHome` so the operator's Inbox catches the spill.
    #[serde(default)]
    pub on_failure: Option<FailureFallback>,

    /// Per-reply override of the transport's degradation matrix.
    /// E.g., a flow that REQUIRES the chart as PNG can set this and
    /// the router refuses to drop to text-only — it surfaces a
    /// `ReplyDegradationRefused` alert instead.
    #[serde(default)]
    pub min_chart_form: Option<ChartFidelity>,
}

/// Where to land the payload when the primary transport refuses.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureFallback {
    /// Post the full-fidelity payload into the operator's Inbox
    /// thread with a banner explaining the original target failed.
    /// Default — work product preserved + alert fired.
    ChatAppendHome,
    /// Surface as alert only (title + truncated text in `detail`).
    /// No chat append.
    AlertOnly,
    /// Silently discard. Used by `Notify`-only flows that don't want
    /// a delivery failure to add to the operator's alert load.
    Drop,
}

impl Default for FailureFallback {
    fn default() -> Self {
        Self::ChatAppendHome
    }
}

/// Minimum acceptable chart-rendering fidelity per the operator's
/// hint. `ReplyDegradationRefused` fires when the chosen transport's
/// caps can't satisfy this.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChartFidelity {
    /// Inline-rendered spec (Vega-embed in the browser). Only the
    /// web transport satisfies this.
    Inline,
    /// Server-rendered raster (PNG/JPEG) sent as attachment. Most
    /// channel transports satisfy this.
    Image,
    /// URL pointing at a rendered image. Lowest acceptable bar.
    Url,
    /// Text-only description. Author opts in by `text_ok` if even
    /// the URL is unacceptable — uncommon.
    TextOk,
}

// ---------------------------------------------------------------------
// Streaming variants — used by the AskAgent → SendReply path and by
// future streaming tools. Same data model as static; the router
// handles either branch via the same per-part packing logic.
// ---------------------------------------------------------------------

/// What flows into the router for a streaming reply. `Static` is the
/// common case (every part ready at `SendReply` time); `Streaming`
/// is used for LLM-token streaming and future tool-side streaming.
///
/// Note: the bus-channel `FlowChannelEvent` enum carries the actual
/// per-delta wire form. This enum is the *handoff shape* from
/// `SendReply` into the router; the router subscribes to the
/// flow-run channel for streaming variants to receive deltas.
#[derive(Debug, Clone)]
pub enum ReplyParts {
    /// All parts known up front.
    Static {
        text: String,
        parts: Vec<ReplyPart>,
    },
    /// Parts arrive as `StreamItem`s on the per-run flow channel.
    /// The router subscribes by `(run_id, node_id)`.
    Streaming {
        run_id: String,
        producer_node_id: String,
        /// Idle-deadline (ms) — after this many ms with no
        /// `StreamItem`, the router declares the stream dead and
        /// flushes whatever it has buffered.
        idle_timeout_ms: u64,
        /// Hard total cap (ms). Stream forcibly terminated at this
        /// point regardless of activity.
        max_duration_ms: u64,
    },
}

/// One incremental update for a streaming reply.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamItem {
    /// Append a fresh part (atomic — appears all at once in renders).
    Part { part: ReplyPart },

    /// Append text to an in-progress text part at `index`. If no
    /// such part exists, create one. The LLM-streaming case maps
    /// onto this: every model token is a `TextDelta { index: 0, text }`.
    TextDelta { index: u32, text: String },

    /// Mark a streaming part complete — flushes pending
    /// renderer-side accumulation buffers.
    PartFinalized { index: u32 },

    /// Stream done — no more items will arrive. Router flushes
    /// buffered content + signals success.
    Done,

    /// Stream errored. Already-emitted deltas stay; the trailer
    /// signals partial-delivery to the renderer.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload_with_parts(parts: Vec<ReplyPart>) -> ReplyPayload {
        ReplyPayload {
            text: "hi".into(),
            parts,
            hints: ReplyHints::default(),
        }
    }

    #[test]
    fn reply_payload_serde_round_trips_minimal() {
        let p = ReplyPayload {
            text: "ok".into(),
            parts: vec![],
            hints: ReplyHints::default(),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: ReplyPayload = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn reply_payload_serde_round_trips_all_part_kinds() {
        let p = payload_with_parts(vec![
            ReplyPart::Attachment {
                attachment_id: "att-1".into(),
                caption: Some("invoice".into()),
            },
            ReplyPart::Artifact {
                artifact_id: "art-1".into(),
                caption: None,
            },
            ReplyPart::Chart {
                spec: json!({"$schema": "https://vega.github.io/schema/vega-lite/v5.json"}),
                theme: Some(ChartTheme::ExeclawDark),
                caption: Some("Spending".into()),
            },
            ReplyPart::Table {
                columns: vec!["date".into(), "amount".into()],
                rows: vec![vec![json!("2026-05-01"), json!(42.50)]],
                caption: None,
            },
            ReplyPart::Card {
                title: "Status".into(),
                fields: vec![CardField {
                    label: "Severity".into(),
                    value: "High".into(),
                    kind: Some("warning".into()),
                }],
            },
            ReplyPart::ExternalFile {
                url: "https://example.com/report.pdf".into(),
                filename: "report.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: Some(12345),
            },
        ]);
        let s = serde_json::to_string(&p).unwrap();
        let back: ReplyPayload = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn reply_hints_defaults_are_sensible() {
        let h = ReplyHints::default();
        assert!(!h.split_per_part);
        assert!(h.on_failure.is_none()); // None = use default (ChatAppendHome)
        assert!(h.min_chart_form.is_none());
        assert_eq!(FailureFallback::default(), FailureFallback::ChatAppendHome);
    }

    #[test]
    fn chart_theme_tag_round_trips() {
        for t in [
            ChartTheme::ExeclawDark,
            ChartTheme::ExeclawLight,
            ChartTheme::Custom {
                config: json!({"axis": {"labelColor": "#fff"}}),
            },
        ] {
            let s = serde_json::to_string(&t).unwrap();
            let back: ChartTheme = serde_json::from_str(&s).unwrap();
            assert_eq!(t.as_str(), back.as_str());
        }
    }

    #[test]
    fn stream_item_serde_round_trips() {
        for item in [
            StreamItem::TextDelta {
                index: 0,
                text: "hello".into(),
            },
            StreamItem::Part {
                part: ReplyPart::Attachment {
                    attachment_id: "a".into(),
                    caption: None,
                },
            },
            StreamItem::PartFinalized { index: 3 },
            StreamItem::Done,
            StreamItem::Error {
                message: "stream timeout".into(),
            },
        ] {
            let s = serde_json::to_string(&item).unwrap();
            let back: StreamItem = serde_json::from_str(&s).unwrap();
            assert_eq!(item, back);
        }
    }

}
