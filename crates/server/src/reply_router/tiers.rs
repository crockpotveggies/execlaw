//! Fallback ladder — produces a sequence of `PreparedReply`
//! candidates ordered most-rich → least-rich. The router tries each
//! in order until one delivers OR all fail.
//!
//! Tiers (per the design doc):
//!   1. **Full** — every part packed per the degrade matrix
//!   2. **Attachments-only** — drop tables/cards, keep text + files
//!   3. **Text + URLs** — drop attachments, inline signed URLs in text
//!   4. **Text-only** — bare text, no attachments
//!
//! Each tier is a strict subset of the previous one's richness, so
//! a transport that rejects tier 1 because of (e.g.) MIME enforcement
//! has a clean fallback path.
//!
//! Where the URL resolvers are stubbed pending the real
//! `download_urls` wiring — for slice 4 we emit a placeholder URL
//! pattern so the tier ladder + delivery tests can run. Slice 5
//! wires the real signed-URL minter.

use super::capabilities::Capabilities;
use super::degrade::{pack_part, DegradationOutcome, PreparedPart};
use execlaw_core::reply::ReplyPayload;

#[derive(Debug, Clone)]
pub struct PreparedReply {
    pub text: String,
    pub parts: Vec<PreparedPart>,
    /// Notes recorded in the flow trace — non-fatal degradation
    /// reasons accumulated during packing.
    pub degradation_notes: Vec<String>,
}

pub fn build_tiers(
    payload: &ReplyPayload,
    caps: &Capabilities,
) -> Vec<(&'static str, PreparedReply)> {
    vec![
        ("tier_full", prepare(payload, caps, TierLevel::Full)),
        (
            "tier_attachments_only",
            prepare(payload, caps, TierLevel::AttachmentsOnly),
        ),
        (
            "tier_text_with_urls",
            prepare(payload, caps, TierLevel::TextWithUrls),
        ),
        ("tier_text_only", prepare(payload, caps, TierLevel::TextOnly)),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TierLevel {
    Full,
    AttachmentsOnly,
    TextWithUrls,
    TextOnly,
}

/// Stub URL resolver used by slice 4. The real signed-URL minter
/// (`download_urls::for_attachment` / `for_artifact`) lands when we
/// wire the router into the live `AppState`. For now this emits a
/// deterministic placeholder so tests can assert payload shapes.
fn stub_url_for_attachment(
    id: &str,
) -> Result<(String, String, String, Option<u64>), String> {
    Ok((
        format!("/api/attachments/{id}?signed=stub"),
        format!("{id}.bin"),
        "application/octet-stream".into(),
        None,
    ))
}

fn stub_url_for_artifact(
    id: &str,
) -> Result<(String, String, String, Option<u64>), String> {
    Ok((
        format!("/api/artifacts/{id}?signed=stub"),
        format!("{id}.bin"),
        "application/octet-stream".into(),
        None,
    ))
}

fn prepare(payload: &ReplyPayload, caps: &Capabilities, tier: TierLevel) -> PreparedReply {
    let mut text = payload.text.clone();
    let mut parts: Vec<PreparedPart> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    for part in &payload.parts {
        // Tier 4: text-only — every part collapses to a one-line text
        // descriptor.
        if tier == TierLevel::TextOnly {
            notes.push(format!("dropped {:?} for text-only tier", part_kind(part)));
            continue;
        }

        // Tier 3: text with URLs — drop tables/cards/charts; keep
        // attachments only as text+url.
        if tier == TierLevel::TextWithUrls {
            match part {
                execlaw_core::reply::ReplyPart::Attachment { .. }
                | execlaw_core::reply::ReplyPart::Artifact { .. }
                | execlaw_core::reply::ReplyPart::ExternalFile { .. } => {
                    // pack via degrade — these will hit the URL
                    // fallback since we'll pretend the transport has
                    // no attachment support at this tier.
                    let bare_caps = make_text_only_caps(caps);
                    apply_pack(part, &bare_caps, &mut text, &mut parts, &mut notes);
                }
                _ => {
                    notes.push(format!(
                        "dropped {:?} for text-with-urls tier",
                        part_kind(part)
                    ));
                }
            }
            continue;
        }

        // Tier 2: attachments-only — keep attachments + files, drop
        // tables/cards/charts.
        if tier == TierLevel::AttachmentsOnly {
            match part {
                execlaw_core::reply::ReplyPart::Attachment { .. }
                | execlaw_core::reply::ReplyPart::Artifact { .. }
                | execlaw_core::reply::ReplyPart::ExternalFile { .. } => {
                    apply_pack(part, caps, &mut text, &mut parts, &mut notes);
                }
                _ => {
                    notes.push(format!(
                        "dropped {:?} for attachments-only tier",
                        part_kind(part)
                    ));
                }
            }
            continue;
        }

        // Tier 1: full.
        apply_pack(part, caps, &mut text, &mut parts, &mut notes);
    }

    // Apply the transport's max_text_length cap.
    if let Some(cap) = caps.max_text_length {
        let cap = cap as usize;
        if text.len() > cap {
            text.truncate(cap.saturating_sub(1));
            text.push('…');
            notes.push(format!("text truncated to {cap} chars"));
        }
    }

    // If the payload had zero text AND zero parts survived
    // degradation, synthesize a one-liner so the operator's reply
    // isn't completely empty.
    if text.trim().is_empty() && parts.is_empty() {
        text = "(reply was empty after degradation)".into();
        notes.push("synthesized empty-reply trailer".into());
    }

    PreparedReply {
        text,
        parts,
        degradation_notes: notes,
    }
}

fn apply_pack(
    part: &execlaw_core::reply::ReplyPart,
    caps: &Capabilities,
    text: &mut String,
    parts: &mut Vec<PreparedPart>,
    notes: &mut Vec<String>,
) {
    match pack_part(part, caps, &stub_url_for_attachment, &stub_url_for_artifact) {
        DegradationOutcome::Prepared(packed) => {
            for p in packed {
                match p {
                    PreparedPart::TextLine(line) => {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push_str("\n\n");
                        }
                        text.push_str(&line);
                    }
                    other => parts.push(other),
                }
            }
        }
        DegradationOutcome::Skipped { reason } => {
            notes.push(format!("part skipped: {reason}"));
        }
        DegradationOutcome::Refused { reason } => {
            notes.push(format!("part refused: {reason}"));
        }
    }
}

/// Strip `supports_attachments` from caps for the text-with-urls
/// tier so the degrade matrix produces TextLine entries instead of
/// Attachment entries.
fn make_text_only_caps(caps: &Capabilities) -> Capabilities {
    let mut c = caps.clone();
    c.supports_attachments = false;
    c.supports_inline_chart = false;
    c.supports_table = false;
    c.supports_card = false;
    c
}

fn part_kind(part: &execlaw_core::reply::ReplyPart) -> &'static str {
    use execlaw_core::reply::ReplyPart;
    match part {
        ReplyPart::Attachment { .. } => "Attachment",
        ReplyPart::Artifact { .. } => "Artifact",
        ReplyPart::Chart { .. } => "Chart",
        ReplyPart::Table { .. } => "Table",
        ReplyPart::Card { .. } => "Card",
        ReplyPart::ExternalFile { .. } => "ExternalFile",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::event_registry::RegisteredReplyHandler;
    use execlaw_core::reply::{CardField, ReplyHints, ReplyPart};
    use serde_json::json;

    fn rich() -> Capabilities {
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

    fn sms() -> Capabilities {
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

    #[test]
    fn build_tiers_returns_four_levels() {
        let p = ReplyPayload {
            text: "hi".into(),
            parts: vec![],
            hints: ReplyHints::default(),
        };
        let tiers = build_tiers(&p, &rich());
        assert_eq!(tiers.len(), 4);
        assert_eq!(tiers[0].0, "tier_full");
        assert_eq!(tiers[3].0, "tier_text_only");
    }

    #[test]
    fn tier_text_only_drops_all_parts() {
        let p = ReplyPayload {
            text: "summary".into(),
            parts: vec![
                ReplyPart::Card {
                    title: "x".into(),
                    fields: vec![CardField {
                        label: "y".into(),
                        value: "z".into(),
                        kind: None,
                    }],
                },
                ReplyPart::Table {
                    columns: vec!["a".into()],
                    rows: vec![],
                    caption: None,
                },
            ],
            hints: ReplyHints::default(),
        };
        let tiers = build_tiers(&p, &rich());
        let text_only = &tiers[3].1;
        assert!(text_only.parts.is_empty());
        assert!(!text_only.degradation_notes.is_empty());
    }

    #[test]
    fn tier_attachments_only_keeps_attachments_drops_table() {
        let p = ReplyPayload {
            text: "summary".into(),
            parts: vec![
                ReplyPart::Attachment {
                    attachment_id: "ok-1".into(),
                    caption: None,
                },
                ReplyPart::Table {
                    columns: vec!["c".into()],
                    rows: vec![],
                    caption: None,
                },
            ],
            hints: ReplyHints::default(),
        };
        let tiers = build_tiers(&p, &rich());
        let attach_only = &tiers[1].1;
        assert_eq!(
            attach_only
                .parts
                .iter()
                .filter(|p| matches!(p, PreparedPart::Attachment { .. }))
                .count(),
            1,
            "attachments-only tier must preserve the attachment"
        );
        assert_eq!(
            attach_only
                .parts
                .iter()
                .filter(|p| matches!(p, PreparedPart::InlineTable { .. }))
                .count(),
            0,
            "attachments-only tier must drop table parts"
        );
    }

    #[test]
    fn text_length_cap_applied() {
        let long = "x".repeat(500);
        let p = ReplyPayload {
            text: long,
            parts: vec![],
            hints: ReplyHints::default(),
        };
        let tiers = build_tiers(&p, &sms());
        let full = &tiers[0].1;
        // SMS cap is 160; we trim to 159 + ellipsis = 160 chars.
        assert!(full.text.chars().count() <= 160);
        assert!(full.text.ends_with('…'));
        assert!(full.degradation_notes.iter().any(|n| n.contains("truncated")));
    }

    #[test]
    fn empty_payload_synthesizes_placeholder() {
        let p = ReplyPayload {
            text: "".into(),
            parts: vec![],
            hints: ReplyHints::default(),
        };
        let tiers = build_tiers(&p, &rich());
        for (_, prepared) in &tiers {
            assert!(!prepared.text.trim().is_empty());
        }
    }

    #[test]
    fn chart_on_rich_transport_lands_as_inline_chart_in_tier_full() {
        let p = ReplyPayload {
            text: "".into(),
            parts: vec![ReplyPart::Chart {
                spec: json!({"mark": "bar"}),
                theme: None,
                caption: Some("spend".into()),
            }],
            hints: ReplyHints::default(),
        };
        let tiers = build_tiers(&p, &rich());
        let full = &tiers[0].1;
        assert!(full
            .parts
            .iter()
            .any(|p| matches!(p, PreparedPart::InlineChart { .. })));
    }
}
