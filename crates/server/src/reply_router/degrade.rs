//! Per-cell degradation logic — converts a `ReplyPart` into one or
//! more `PreparedPart` values that the transport can render
//! directly, given a handler's `Capabilities`.
//!
//! The matrix (one cell per (PartKind, Transport-Capability)) is
//! reified as a function `pack_part`. The router calls this for
//! each `ReplyPart` in the payload, then concatenates the prepared
//! output before handing to the handler.
//!
//! Where chart rasterization is needed today we degrade to a URL
//! pointing at the SPA's renderer (`/api/charts/render?spec=...`)
//! — rendering happens client-side via Vega-Lite. Server-side raster
//! via `vl-convert-rs` is deferred: it pulls a 40 MB Deno bundle
//! and isn't needed until we want to send charts to truly
//! transport-only channels (SMS, voice). Today's WhatsApp / Signal
//! plugins use the SPA-render URL for previews instead.

use super::capabilities::Capabilities;
use execlaw_core::reply::{CardField, ChartTheme, ReplyPart};

/// One unit of prepared output the handler can deliver verbatim.
/// The handler renders these in order (text first, then attachments,
/// then auxiliary parts).
#[derive(Debug, Clone, PartialEq)]
pub enum PreparedPart {
    /// Append to the reply's text body. The router accumulates all
    /// `TextLine` parts into one text blob before sending.
    TextLine(String),
    /// Reference an existing AttachmentRow / ArtifactRow by id +
    /// (optionally) a signed download URL for transports that take
    /// URLs rather than blob references.
    Attachment {
        kind: AttachmentRefKind,
        id: String,
        url: String,
        filename: String,
        mime_type: String,
        caption: Option<String>,
        size_bytes: Option<u64>,
    },
    /// Structured card payload for rich transports.
    Card {
        title: String,
        fields: Vec<CardField>,
    },
    /// Inline chart spec for transports that can render Vega-Lite
    /// directly (web). Carries the theme so the handler doesn't
    /// have to re-merge.
    InlineChart {
        spec: serde_json::Value,
        theme: ChartTheme,
        caption: Option<String>,
    },
    /// Inline table data for transports with a table component.
    InlineTable {
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
        caption: Option<String>,
    },
}

/// Whether an attachment refers to the conversation-scoped table
/// (`state_attachments`) or the artifact table
/// (`state_attachment_artifacts` — plugin-emitted, may have TTL).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AttachmentRefKind {
    Attachment,
    Artifact,
}

/// Outcome of degrading one `ReplyPart`. Most paths produce a
/// `Prepared(Vec<PreparedPart>)`; the `Refused` variant fires when
/// the operator's `ReplyHints::min_chart_form` requires more
/// fidelity than the transport supports.
#[derive(Debug)]
pub enum DegradationOutcome {
    Prepared(Vec<PreparedPart>),
    /// Hint violated — emit a `ReplyDegradationRefused` alert and
    /// fail the route.
    Refused { reason: String },
    /// Part references content that no longer exists (e.g.,
    /// attachment id that the retention sweeper raced). The router
    /// drops the part and notes the reason in the trace.
    Skipped { reason: String },
}

/// Convert one `ReplyPart` into delivery-ready chunks for the given
/// transport. Pure function — no DB access — so unit tests can
/// exhaustively cover the matrix.
///
/// The `signed_url_for_attachment` / `signed_url_for_artifact`
/// closures are injected so this stays test-friendly (real router
/// passes closures backed by the `download_urls` subsystem).
pub fn pack_part<FA, FR>(
    part: &ReplyPart,
    caps: &Capabilities,
    signed_url_for_attachment: &FA,
    signed_url_for_artifact: &FR,
) -> DegradationOutcome
where
    FA: Fn(&str) -> Result<(String, String, String, Option<u64>), String>, // (url, filename, mime, size)
    FR: Fn(&str) -> Result<(String, String, String, Option<u64>), String>,
{
    match part {
        ReplyPart::Attachment {
            attachment_id,
            caption,
        } => {
            let (url, filename, mime, size) = match signed_url_for_attachment(attachment_id) {
                Ok(v) => v,
                Err(e) => {
                    return DegradationOutcome::Skipped {
                        reason: format!("attachment {attachment_id} not resolvable: {e}"),
                    };
                }
            };
            pack_blob(
                AttachmentRefKind::Attachment,
                attachment_id,
                url,
                filename,
                mime,
                size,
                caption.clone(),
                caps,
            )
        }
        ReplyPart::Artifact {
            artifact_id,
            caption,
        } => {
            let (url, filename, mime, size) = match signed_url_for_artifact(artifact_id) {
                Ok(v) => v,
                Err(e) => {
                    return DegradationOutcome::Skipped {
                        reason: format!("artifact {artifact_id} not resolvable: {e}"),
                    };
                }
            };
            pack_blob(
                AttachmentRefKind::Artifact,
                artifact_id,
                url,
                filename,
                mime,
                size,
                caption.clone(),
                caps,
            )
        }
        ReplyPart::Chart {
            spec,
            theme,
            caption,
        } => {
            if caps.supports_inline_chart {
                let resolved_theme = theme.clone().unwrap_or(ChartTheme::ExeclawDark);
                return DegradationOutcome::Prepared(vec![PreparedPart::InlineChart {
                    spec: spec.clone(),
                    theme: resolved_theme,
                    caption: caption.clone(),
                }]);
            }
            // Server-side raster path is deferred (no vl-convert-rs
            // yet). Degrade to a text line referencing the chart by
            // caption, plus (TODO slice 4b) a signed URL pointing at
            // the SPA's renderer. For now we emit just the caption so
            // poor-transport delivery doesn't carry a stale URL.
            let label = caption.as_deref().unwrap_or("chart");
            DegradationOutcome::Prepared(vec![PreparedPart::TextLine(format!(
                "📊 {label} (rendered chart available in the web UI)"
            ))])
        }
        ReplyPart::Table {
            columns,
            rows,
            caption,
        } => {
            if caps.supports_table {
                return DegradationOutcome::Prepared(vec![PreparedPart::InlineTable {
                    columns: columns.clone(),
                    rows: rows.clone(),
                    caption: caption.clone(),
                }]);
            }
            // Flatten to text rows. Cap at 10 rows so we don't blow
            // through the transport's text-length budget.
            let lines = flatten_table(columns, rows, caption.as_deref(), 10);
            DegradationOutcome::Prepared(vec![PreparedPart::TextLine(lines)])
        }
        ReplyPart::Card { title, fields } => {
            if caps.supports_card {
                return DegradationOutcome::Prepared(vec![PreparedPart::Card {
                    title: title.clone(),
                    fields: fields.clone(),
                }]);
            }
            // Flatten to "title\n  label: value\n  …" text.
            let mut s = format!("**{title}**\n");
            for f in fields {
                s.push_str(&format!("  • {}: {}\n", f.label, f.value));
            }
            DegradationOutcome::Prepared(vec![PreparedPart::TextLine(s)])
        }
        ReplyPart::ExternalFile {
            url,
            filename,
            mime_type,
            size_bytes,
        } => {
            if !caps.mime_allowed(mime_type) {
                return DegradationOutcome::Skipped {
                    reason: format!(
                        "mime '{mime_type}' not in handler's allowed prefixes"
                    ),
                };
            }
            if caps.supports_attachments
                && size_bytes.map(|s| within_size_cap(s, caps)).unwrap_or(true)
            {
                return DegradationOutcome::Prepared(vec![PreparedPart::Attachment {
                    kind: AttachmentRefKind::Artifact, // external = uncached at this stage; TODO cache
                    id: format!("ext::{url}"),
                    url: url.clone(),
                    filename: filename.clone(),
                    mime_type: mime_type.clone(),
                    caption: None,
                    size_bytes: *size_bytes,
                }]);
            }
            DegradationOutcome::Prepared(vec![PreparedPart::TextLine(format!(
                "📎 {filename} — {url}"
            ))])
        }
    }
}

fn pack_blob(
    kind: AttachmentRefKind,
    id: &str,
    url: String,
    filename: String,
    mime: String,
    size: Option<u64>,
    caption: Option<String>,
    caps: &Capabilities,
) -> DegradationOutcome {
    if !caps.mime_allowed(&mime) {
        return DegradationOutcome::Skipped {
            reason: format!("mime '{mime}' not in handler's allowed prefixes"),
        };
    }
    if caps.supports_attachments && size.map(|s| within_size_cap(s, caps)).unwrap_or(true) {
        return DegradationOutcome::Prepared(vec![PreparedPart::Attachment {
            kind,
            id: id.to_owned(),
            url,
            filename,
            mime_type: mime,
            caption,
            size_bytes: size,
        }]);
    }
    // No attachment support OR too large — URL fallback.
    let cap = caption.as_deref().unwrap_or(&filename);
    DegradationOutcome::Prepared(vec![PreparedPart::TextLine(format!("📎 {cap} — {url}"))])
}

fn within_size_cap(size: u64, caps: &Capabilities) -> bool {
    caps.max_attachment_size_bytes.map_or(true, |max| size <= max)
}

fn flatten_table(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    caption: Option<&str>,
    row_cap: usize,
) -> String {
    let mut out = String::new();
    if let Some(c) = caption {
        out.push_str(&format!("**{c}**\n"));
    }
    out.push_str(&columns.join(" | "));
    out.push('\n');
    out.push_str(&columns.iter().map(|_| "---").collect::<Vec<_>>().join(" | "));
    out.push('\n');
    let shown = rows.len().min(row_cap);
    for row in rows.iter().take(shown) {
        let cells: Vec<String> = row.iter().map(value_to_cell).collect();
        out.push_str(&cells.join(" | "));
        out.push('\n');
    }
    if rows.len() > row_cap {
        out.push_str(&format!("…and {} more rows", rows.len() - row_cap));
    }
    out
}

fn value_to_cell(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::event_registry::RegisteredReplyHandler;
    use serde_json::json;

    fn rich_caps() -> Capabilities {
        Capabilities::from_registered(RegisteredReplyHandler {
            name: "web".into(),
            plugin_id: "core".into(),
            description: "".into(),
            supports_streaming: true,
            supports_attachments: true,
            supports_inline_chart: true,
            supports_table: true,
            supports_card: true,
            supports_markdown: true,
            max_attachment_size_bytes: None,
            max_attachments_per_message: None,
            max_text_length: None,
            allowed_mime_prefixes: None,
        })
    }

    fn whatsapp_caps() -> Capabilities {
        Capabilities::from_registered(RegisteredReplyHandler {
            name: "whatsapp".into(),
            plugin_id: "whatsapp".into(),
            description: "".into(),
            supports_streaming: false,
            supports_attachments: true,
            supports_inline_chart: false,
            supports_table: false,
            supports_card: false,
            supports_markdown: true,
            max_attachment_size_bytes: Some(16_777_216),
            max_attachments_per_message: Some(1),
            max_text_length: Some(4096),
            allowed_mime_prefixes: Some(vec!["image/".into(), "application/pdf".into()]),
        })
    }

    fn sms_caps() -> Capabilities {
        Capabilities::from_registered(RegisteredReplyHandler {
            name: "sms".into(),
            plugin_id: "sms".into(),
            description: "".into(),
            supports_streaming: false,
            supports_attachments: false,
            supports_inline_chart: false,
            supports_table: false,
            supports_card: false,
            supports_markdown: false,
            max_text_length: Some(160),
            max_attachment_size_bytes: None,
            max_attachments_per_message: None,
            allowed_mime_prefixes: None,
        })
    }

    // Stub URL resolver — succeeds for ids that start with "ok",
    // fails for "missing".
    fn stub_resolver(
        id: &str,
    ) -> Result<(String, String, String, Option<u64>), String> {
        if id.starts_with("missing") {
            return Err("not found".into());
        }
        Ok((
            format!("https://example.com/d/{id}"),
            "file.bin".into(),
            "application/octet-stream".into(),
            Some(1024),
        ))
    }

    fn big_resolver(_id: &str) -> Result<(String, String, String, Option<u64>), String> {
        // 50 MB image — mime IS allowed by WhatsApp caps, but the size
        // cap (16 MB) forces the URL fallback path.
        Ok((
            "https://example.com/big".into(),
            "huge.jpg".into(),
            "image/jpeg".into(),
            Some(50_000_000),
        ))
    }

    fn image_resolver(_id: &str) -> Result<(String, String, String, Option<u64>), String> {
        Ok((
            "https://example.com/img".into(),
            "photo.jpg".into(),
            "image/jpeg".into(),
            Some(2_000_000),
        ))
    }

    #[test]
    fn attachment_with_rich_transport_becomes_inline_attachment() {
        let part = ReplyPart::Attachment {
            attachment_id: "ok-1".into(),
            caption: Some("invoice".into()),
        };
        let out = pack_part(&part, &rich_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => {
                assert!(matches!(p[0], PreparedPart::Attachment { .. }));
            }
            other => panic!("expected Prepared, got {other:?}"),
        }
    }

    #[test]
    fn attachment_with_no_attachment_support_falls_to_url_in_text() {
        let part = ReplyPart::Attachment {
            attachment_id: "ok-2".into(),
            caption: Some("report".into()),
        };
        let out = pack_part(&part, &sms_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => match &p[0] {
                PreparedPart::TextLine(s) => assert!(s.contains("report") && s.contains("https://")),
                other => panic!("expected TextLine, got {other:?}"),
            },
            other => panic!("expected Prepared, got {other:?}"),
        }
    }

    #[test]
    fn attachment_too_large_falls_to_url() {
        let part = ReplyPart::Attachment {
            attachment_id: "huge".into(),
            caption: None,
        };
        let out = pack_part(&part, &whatsapp_caps(), &big_resolver, &big_resolver);
        match out {
            DegradationOutcome::Prepared(p) => match &p[0] {
                PreparedPart::TextLine(_) => {}
                other => panic!("expected TextLine, got {other:?}"),
            },
            other => panic!("expected Prepared, got {other:?}"),
        }
    }

    #[test]
    fn attachment_with_bad_mime_skipped() {
        let part = ReplyPart::Attachment {
            attachment_id: "ok-3".into(),
            caption: None,
        };
        // whatsapp_caps allows image/* and pdf only; stub_resolver
        // returns application/octet-stream → skipped.
        let out = pack_part(&part, &whatsapp_caps(), &stub_resolver, &stub_resolver);
        assert!(matches!(out, DegradationOutcome::Skipped { .. }));
    }

    #[test]
    fn attachment_with_allowed_mime_passes() {
        let part = ReplyPart::Attachment {
            attachment_id: "ok-4".into(),
            caption: None,
        };
        let out = pack_part(&part, &whatsapp_caps(), &image_resolver, &image_resolver);
        assert!(matches!(out, DegradationOutcome::Prepared(_)));
    }

    #[test]
    fn missing_attachment_id_skipped_not_panicked() {
        let part = ReplyPart::Attachment {
            attachment_id: "missing-x".into(),
            caption: None,
        };
        let out = pack_part(&part, &rich_caps(), &stub_resolver, &stub_resolver);
        assert!(matches!(out, DegradationOutcome::Skipped { .. }));
    }

    #[test]
    fn chart_rich_transport_inline() {
        let part = ReplyPart::Chart {
            spec: json!({"mark": "bar"}),
            theme: Some(ChartTheme::ExeclawDark),
            caption: Some("spend".into()),
        };
        let out = pack_part(&part, &rich_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => {
                assert!(matches!(p[0], PreparedPart::InlineChart { .. }));
            }
            _ => panic!("chart should be inline on rich transports"),
        }
    }

    #[test]
    fn chart_poor_transport_falls_to_text() {
        let part = ReplyPart::Chart {
            spec: json!({"mark": "bar"}),
            theme: None,
            caption: Some("spend".into()),
        };
        let out = pack_part(&part, &whatsapp_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => match &p[0] {
                PreparedPart::TextLine(s) => assert!(s.contains("spend")),
                _ => panic!("expected TextLine"),
            },
            _ => panic!("expected Prepared"),
        }
    }

    #[test]
    fn table_poor_transport_flattens_to_text() {
        let part = ReplyPart::Table {
            columns: vec!["date".into(), "amount".into()],
            rows: vec![
                vec![json!("2026-05-01"), json!(42)],
                vec![json!("2026-05-02"), json!(99)],
            ],
            caption: Some("bills".into()),
        };
        let out = pack_part(&part, &sms_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => match &p[0] {
                PreparedPart::TextLine(s) => {
                    assert!(s.contains("bills"));
                    assert!(s.contains("date | amount"));
                    assert!(s.contains("2026-05-01"));
                }
                _ => panic!("expected TextLine"),
            },
            _ => panic!("expected Prepared"),
        }
    }

    #[test]
    fn card_rich_transport_preserves_card_shape() {
        let part = ReplyPart::Card {
            title: "Status".into(),
            fields: vec![CardField {
                label: "Severity".into(),
                value: "High".into(),
                kind: Some("warning".into()),
            }],
        };
        let out = pack_part(&part, &rich_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => {
                assert!(matches!(p[0], PreparedPart::Card { .. }));
            }
            _ => panic!("expected Prepared::Card"),
        }
    }

    #[test]
    fn card_poor_transport_flattens_to_markdown_text() {
        let part = ReplyPart::Card {
            title: "Status".into(),
            fields: vec![CardField {
                label: "Severity".into(),
                value: "High".into(),
                kind: None,
            }],
        };
        let out = pack_part(&part, &sms_caps(), &stub_resolver, &stub_resolver);
        match out {
            DegradationOutcome::Prepared(p) => match &p[0] {
                PreparedPart::TextLine(s) => {
                    assert!(s.contains("Status"));
                    assert!(s.contains("Severity"));
                    assert!(s.contains("High"));
                }
                _ => panic!("expected TextLine"),
            },
            _ => panic!("expected Prepared"),
        }
    }
}
