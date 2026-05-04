//! On-disk workspace for one deep-research job.
//!
//! Each job gets `<root>/<job_id>/` containing:
//!
//!   * `plan.json` — pretty-printed planner output (human-readable so
//!     operators can grep it).
//!   * `notes/<n>.json` — per-sub-query notes (C4).
//!   * `report.md` — final synthesized report (C5).
//!
//! Filesystem (not encrypted DB) so reports stay greppable +
//! publishable. The `state_research_jobs` row carries the index;
//! the workspace holds bulky payloads.
//!
//! 2026-04-29.

use execlaw_core::ids::ResearchJobId;
use execlaw_core::research::{ResearchNote, ResearchPlan};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding: {0}")]
    Encoding(String),
}

/// Operator-configurable workspace root. Defaults to
/// `~/.execlaw/research/`. The directory is created on first use.
#[derive(Debug, Clone)]
pub struct ResearchWorkspace {
    root: PathBuf,
}

impl ResearchWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default root: `<home>/.execlaw/research/` if home is
    /// resolvable; otherwise a process-local fallback under the
    /// current working directory so dev-server invocations without a
    /// resolvable home still work.
    pub fn default_root() -> PathBuf {
        match directories::UserDirs::new() {
            Some(d) => d.home_dir().join(".execlaw").join("research"),
            None => PathBuf::from(".execlaw").join("research"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the per-job dir + `notes/` subdir. Returns the absolute
    /// path. Idempotent — calling twice doesn't error.
    pub fn provision(&self, job_id: &ResearchJobId) -> Result<PathBuf, WorkspaceError> {
        let dir = self.root.join(job_id.as_str());
        std::fs::create_dir_all(dir.join("notes"))?;
        Ok(dir)
    }

    /// Write the planner output as pretty-printed JSON. The plan is
    /// also persisted in `state_research_jobs.plan_json` for the
    /// fast-path read; this file is the operator-grep view.
    pub fn write_plan(
        &self,
        job_id: &ResearchJobId,
        plan: &ResearchPlan,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        let path = dir.join("plan.json");
        let body = serde_json::to_string_pretty(plan)
            .map_err(|e| WorkspaceError::Encoding(e.to_string()))?;
        std::fs::write(&path, body)?;
        Ok(path)
    }

    /// Write one gather-phase note as `notes/<index>.json`. Pretty-
    /// printed so operators can grep it directly. Idempotent —
    /// re-writing the same index overwrites cleanly (a worker that
    /// retries lands here without duplicate files).
    pub fn write_note(
        &self,
        job_id: &ResearchJobId,
        note: &ResearchNote,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        let path = dir.join("notes").join(format!("{}.json", note.index));
        let body = serde_json::to_string_pretty(note)
            .map_err(|e| WorkspaceError::Encoding(e.to_string()))?;
        std::fs::write(&path, body)?;
        Ok(path)
    }

    /// Write the synthesized report to `report.md` (C5). Returns the
    /// absolute path; callers register that path with the
    /// `AttachmentStore` so transport plugins can `send_file` it on
    /// the CardClosed event.
    pub fn write_report(
        &self,
        job_id: &ResearchJobId,
        markdown: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        let path = dir.join("report.md");
        std::fs::write(&path, markdown)?;
        Ok(path)
    }

    /// 2026-05-03 — render the synthesized markdown report into a
    /// `report.pdf` alongside `report.md` so the operator's
    /// CardClosed deliverable is the PDF (per MIGRATION_PLAN §5.6
    /// "PDF" + "DELIVER" steps). Returns the absolute path on
    /// success. Pure-Rust pipeline — `printpdf` ships the 14
    /// standard PDF fonts (Helvetica family) so we don't bundle any
    /// TTFs and the operator host needs no external deps.
    ///
    /// Markdown subset rendered today:
    ///   * `# Heading` → 18pt bold
    ///   * `## Heading` → 14pt bold
    ///   * `### Heading` → 12pt bold
    ///   * `- item` / `* item` → bullet, 11pt regular
    ///   * Blank line → paragraph break
    ///   * Everything else → 11pt regular paragraph (word-wrapped)
    ///
    /// Inline emphasis (`*italic*`, `**bold**`), code blocks, and
    /// tables degrade to plain text — fine for v1 since the
    /// synthesizer's prompt produces structured prose, not richly
    /// formatted markdown.
    pub fn write_report_pdf(
        &self,
        job_id: &ResearchJobId,
        markdown: &str,
        title: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.provision(job_id)?;
        // 2026-05-04 — friendly filename. Was: hard-coded
        // "report.pdf" (the user's save-as dialog showed
        // "report.pdf" for every research deliverable, regardless
        // of topic). Now derived from the markdown's first H1
        // (the synthesizer's article title) — slugified to a
        // 3-5 word stub, suffixed with today's date in ISO
        // YYYY-MM-DD format. Falls back to a sanitised version of
        // the `title` parameter when the markdown has no usable H1.
        let stem = derive_report_filename_stem(markdown, title);
        let date = chrono::Utc::now().format("%Y-%m-%d");
        let filename = format!("{stem}-{date}.pdf");
        let path = dir.join(filename);
        render_markdown_to_pdf(markdown, title, &path)
            .map_err(|e| WorkspaceError::Encoding(format!("pdf render: {e}")))?;
        Ok(path)
    }

    /// Read the synthesized report back. Used by the
    /// `research_get_report` tool. Returns `Ok(None)` when the file
    /// hasn't been written yet (gather phase still running, or job
    /// failed before synthesize). Other I/O errors propagate.
    pub fn read_report(&self, job_id: &ResearchJobId) -> Result<Option<String>, WorkspaceError> {
        let path = self.root.join(job_id.as_str()).join("report.md");
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::Io(e)),
        }
    }

    /// Tear down a job's workspace. C6's retention sweeper calls this
    /// after the row's terminal `finished_at` ages past the global
    /// retention cutoff. Safe to call when the dir doesn't exist.
    pub fn purge(&self, job_id: &ResearchJobId) -> Result<(), WorkspaceError> {
        let dir = self.root.join(job_id.as_str());
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

// -----------------------------------------------------------------
// Phase D PDF rendering — pure-Rust markdown subset → PDF
// -----------------------------------------------------------------

use printpdf::{BuiltinFont, IndirectFontRef, Mm, PdfDocument, PdfDocumentReference, PdfLayerReference, PdfPageIndex};

const PAGE_W: f32 = 216.0; // mm — US Letter
const PAGE_H: f32 = 279.0;
const MARGIN_X: f32 = 20.0;
const MARGIN_TOP: f32 = 20.0;
const MARGIN_BOTTOM: f32 = 20.0;
const LINE_H_BODY: f32 = 5.0;
const LINE_H_HEADING: f32 = 8.0;
const PARA_GAP: f32 = 3.0;
const WRAP_WIDTH: usize = 95; // chars at 11pt Helvetica fits ~95 across (216-40)mm

/// Render a small markdown subset to a single-or-multi-page PDF
/// at `out_path`. See `Workspace::write_report_pdf` for the
/// supported syntax.
fn render_markdown_to_pdf(
    markdown: &str,
    title: &str,
    out_path: &std::path::Path,
) -> Result<(), String> {
    let (doc, page1, layer1) = PdfDocument::new(title, Mm(PAGE_W), Mm(PAGE_H), "Layer 1");
    let font_regular = doc
        .add_builtin_font(BuiltinFont::Helvetica)
        .map_err(|e| format!("regular font: {e}"))?;
    let font_bold = doc
        .add_builtin_font(BuiltinFont::HelveticaBold)
        .map_err(|e| format!("bold font: {e}"))?;

    let mut page = page1;
    let mut layer_id = layer1;
    let mut layer = doc.get_page(page).get_layer(layer_id);
    let mut y = PAGE_H - MARGIN_TOP;
    // Track whether we just landed on a fresh page so the
    // "above-heading" gap doesn't waste space at the top of a
    // page (the heading is already sitting at the top margin).
    let mut at_page_top = true;

    for raw in markdown.lines() {
        let line = raw.trim_end_matches('\r');

        // Blank line → paragraph gap.
        if line.trim().is_empty() {
            y -= PARA_GAP;
            continue;
        }

        // Strip emphasis markers (rough): we don't do italics, but
        // dropping the asterisks keeps the text readable instead
        // of showing literal "**" everywhere.
        let cleaned = strip_emphasis(line);

        // Classify the line.
        //
        // 2026-05-04 — header handling now covers h1 through h6
        // (was: only h1/h2/h3, with `#### Foo` being silently
        // rendered as h3-with-text-"# Foo" because the `### `
        // prefix matched first). Order matters: check the LONGEST
        // marker first so `#### ` doesn't accidentally swallow as
        // `### ` + text.
        //
        // Each header carries an `above_gap` — extra vertical
        // breathing room before the line — that scales with the
        // heading's importance. Higher-level headers (h1, h2)
        // get more space above so the document's structure reads
        // visually instead of running together. The gap is
        // skipped when the heading lands at the top of a page
        // (else it'd waste the top margin's worth of paper).
        let header = classify_header(&cleaned);
        let (text, font, size, line_h, above_gap) = if let Some(h) = header {
            (h.text, &font_bold, h.size_pt, h.line_h, h.above_gap)
        } else if let Some(rest) = cleaned
            .strip_prefix("- ")
            .or_else(|| cleaned.strip_prefix("* "))
        {
            (format!("• {rest}"), &font_regular, 11.0, LINE_H_BODY, 0.0)
        } else {
            (cleaned, &font_regular, 11.0, LINE_H_BODY, 0.0)
        };

        // Apply the above-heading gap (skipped on a fresh page).
        if above_gap > 0.0 && !at_page_top {
            y -= above_gap;
        }

        // Word-wrap and emit each visual line.
        let wrap_w = if size >= 14.0 { WRAP_WIDTH * 7 / 10 } else { WRAP_WIDTH };
        for visual in textwrap::wrap(&text, wrap_w) {
            if y - line_h < MARGIN_BOTTOM {
                let (np, nl) = doc.add_page(Mm(PAGE_W), Mm(PAGE_H), "Layer");
                page = np;
                layer_id = nl;
                layer = doc.get_page(page).get_layer(layer_id);
                y = PAGE_H - MARGIN_TOP;
                at_page_top = true;
            }
            layer.use_text(visual.as_ref(), size, Mm(MARGIN_X), Mm(y), font);
            y -= line_h;
            at_page_top = false;
        }
    }

    let f = std::fs::File::create(out_path).map_err(|e| format!("create: {e}"))?;
    let mut writer = std::io::BufWriter::new(f);
    doc.save(&mut writer).map_err(|e| format!("save: {e}"))?;
    Ok(())
}

/// Build the filename stem (the part before `-<date>.pdf`) for a
/// research report. Picks the first usable source in this order:
///   1. The markdown's first H1 line (the synthesiser's article
///      title — best signal for what the report is actually about)
///   2. The provided `title` parameter (caller-set, currently
///      "Research report — <job-id>")
///   3. Hard fallback "research-report" so we never produce an
///     empty filename
///
/// Slugified: lowercase ASCII, words joined by `-`, capped at
/// 5 words and `MAX_STEM_CHARS` characters. Stop-words trimmed
/// from the front so "the-best-evergreen-ground-covers" becomes
/// "best-evergreen-ground-covers" — keeps the operator's
/// download-list scannable.
fn derive_report_filename_stem(markdown: &str, title: &str) -> String {
    const MAX_WORDS: usize = 5;
    const MAX_STEM_CHARS: usize = 60;
    const STOP_WORDS: &[&str] = &[
        "the", "a", "an", "of", "for", "to", "and", "or", "with", "in", "on", "at", "by",
    ];

    let raw = first_h1(markdown)
        .or_else(|| {
            // The runner's title is currently
            // "Research report — <job-id>". Strip the boilerplate
            // prefix; what's left is either the job-id (useless,
            // produces "research-report" hard fallback below) or a
            // future caller's actual title.
            let t = title.trim();
            if let Some(after) = t
                .strip_prefix("Research report — ")
                .or_else(|| t.strip_prefix("Research report - "))
                .or_else(|| t.strip_prefix("Research report: "))
            {
                Some(after.to_owned())
            } else {
                Some(t.to_owned())
            }
        })
        .unwrap_or_else(|| "research-report".to_owned());

    // Tokenize: lowercase, alphanumerics-only, words separated by
    // any non-alnum run.
    let lowered = raw.to_lowercase();
    let mut words: Vec<String> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_owned())
        .collect();

    // Drop leading stop-words.
    while let Some(first) = words.first() {
        if STOP_WORDS.contains(&first.as_str()) {
            words.remove(0);
        } else {
            break;
        }
    }

    // Cap at MAX_WORDS, then re-cap at MAX_STEM_CHARS in case
    // those 5 words are unusually long.
    words.truncate(MAX_WORDS);
    if words.is_empty() {
        return "research-report".to_owned();
    }
    let mut stem = words.join("-");
    if stem.chars().count() > MAX_STEM_CHARS {
        stem = stem.chars().take(MAX_STEM_CHARS).collect();
        // Trim a partial trailing word so we don't end mid-word.
        if let Some(last_dash) = stem.rfind('-') {
            if last_dash > 8 {
                // keep stem readable
                stem.truncate(last_dash);
            }
        }
    }
    stem
}

/// Pull the first `# Heading` line out of a markdown document.
/// Returns None if the doc has no level-1 heading. We deliberately
/// only consider H1 (not H2) because synthesised reports always
/// open with the article title at H1; H2/H3 are sub-section
/// headings and would produce off-topic filenames.
fn first_h1(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# ") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

/// Per-level heading metadata. The renderer reads this to size +
/// space each `#`-prefixed line.
struct HeaderSpec {
    text: String,
    /// Point size for the heading line. Bigger for h1.
    size_pt: f32,
    /// Line height (mm) — controls intra-heading wrap spacing.
    line_h: f32,
    /// Extra mm of vertical space inserted ABOVE the heading.
    /// Higher-level headings (h1, h2) get more so the document
    /// structure reads visually. Skipped at page top.
    above_gap: f32,
}

/// Classify a line as a markdown heading (h1-h6) and return its
/// rendered spec. Returns `None` for non-headings (the caller
/// falls through to bullet / body handling).
///
/// Order is important: longest prefix first. Without this,
/// `#### Foo` matches `### ` first → renders as h3 with text
/// "# Foo" which is the bug the user reported as "not all
/// markdown headers are rendering."
fn classify_header(line: &str) -> Option<HeaderSpec> {
    // Each tuple: (prefix, size_pt, line_h, above_gap_mm)
    // Sizes shrink on lower levels; gaps shrink correspondingly.
    // h5 / h6 keep bold but are barely larger than body so they
    // don't visually compete with h3 / h4.
    const LEVELS: &[(&str, f32, f32, f32)] = &[
        ("###### ", 10.0, LINE_H_BODY, 3.0),
        ("##### ", 11.0, LINE_H_BODY, 4.0),
        ("#### ", 12.0, LINE_H_BODY + 1.0, 5.0),
        ("### ", 13.0, LINE_H_HEADING - 1.0, 7.0),
        ("## ", 15.0, LINE_H_HEADING, 9.0),
        ("# ", 18.0, LINE_H_HEADING + 2.0, 12.0),
    ];
    for &(prefix, size_pt, line_h, above_gap) in LEVELS {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(HeaderSpec {
                text: rest.to_string(),
                size_pt,
                line_h,
                above_gap,
            });
        }
    }
    None
}

fn strip_emphasis(s: &str) -> String {
    // Drop `**` and `__` (bold) and single `*` / `_` (italic)
    // markers. Preserves everything else. Leaves backtick code
    // spans intact since the model rarely uses them in synthesized
    // prose and they read fine verbatim.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && (bytes[i] == b'*' && bytes[i + 1] == b'*') {
            i += 2;
            continue;
        }
        if i + 1 < bytes.len() && (bytes[i] == b'_' && bytes[i + 1] == b'_') {
            i += 2;
            continue;
        }
        // Single * or _ used as emphasis: strip when adjacent to a
        // word character on at least one side. That covers both the
        // opening (`*emphasis`) and closing (`emphasis*`) markers
        // while leaving bullet markers (`* item`, where the `*` has
        // space on both sides) and lone-punctuation `*` alone.
        if bytes[i] == b'*' || bytes[i] == b'_' {
            let left_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let right_word = i + 1 < bytes.len()
                && (bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_');
            if left_word || right_word {
                i += 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// Type aliases used only to keep the public function signature
// readable; nothing depends on them externally.
#[allow(dead_code)]
type _PdfRefs = (PdfDocumentReference, PdfPageIndex, PdfLayerReference, IndirectFontRef);

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::research::PlanStep;

    #[test]
    fn provision_creates_job_and_notes_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let dir = ws.provision(&id).unwrap();
        assert!(dir.exists());
        assert!(dir.join("notes").exists());
    }

    #[test]
    fn provision_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let first = ws.provision(&id).unwrap();
        let second = ws.provision(&id).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn write_plan_round_trips_pretty_json() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let plan = ResearchPlan {
            thesis: "thesis".into(),
            steps: vec![PlanStep {
                query: "first".into(),
                rationale: Some("baseline".into()),
            }],
        };
        let path = ws.write_plan(&id, &plan).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"thesis\""));
        assert!(body.contains("\"first\""));
        // Newlines mean we got the pretty-printed form.
        assert!(body.contains('\n'));
    }

    #[test]
    fn write_note_lands_in_notes_subdir_and_overwrites_idempotently() {
        use execlaw_core::research::{ResearchNote, ResearchSource, SubQueryState};
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let note = ResearchNote {
            index: 3,
            sub_query: "anything".into(),
            state: SubQueryState::Done,
            excerpt: "v1".into(),
            sources: vec![ResearchSource {
                url: "https://example.com".into(),
                title: Some("ex".into()),
                fetched_ok: true,
                error: None,
            }],
            tokens_used: Some(42),
            error: None,
        };
        let path = ws.write_note(&id, &note).unwrap();
        assert!(path.ends_with("notes/3.json") || path.ends_with("notes\\3.json"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"v1\""));
        // Re-write with a different excerpt — must overwrite, not error.
        let note2 = ResearchNote {
            excerpt: "v2".into(),
            ..note
        };
        ws.write_note(&id, &note2).unwrap();
        let body2 = std::fs::read_to_string(&path).unwrap();
        assert!(body2.contains("\"v2\""));
        assert!(!body2.contains("\"v1\""));
    }

    #[test]
    fn purge_removes_workspace_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        ws.provision(&id).unwrap();
        ws.purge(&id).unwrap();
        assert!(!tmp.path().join(id.as_str()).exists());
    }

    #[test]
    fn purge_is_safe_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        // No provision call — dir doesn't exist. Purge must not error.
        ws.purge(&id).unwrap();
    }

    // ---------------- Phase D PDF render ----------------

    #[test]
    fn write_report_pdf_writes_a_pdf_file_with_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let md = "# Title\n\n## Subhead\n\nA short paragraph that should wrap reasonably and produce at least one line of body text.\n\n- Bullet one\n- Bullet two";
        let path = ws.write_report_pdf(&id, md, "Test report").unwrap();
        assert!(path.exists(), "report.pdf must land on disk");
        let bytes = std::fs::read(&path).unwrap();
        // PDF magic: %PDF-
        assert!(bytes.len() > 200, "rendered pdf should be non-trivial");
        assert_eq!(&bytes[0..5], b"%PDF-", "must have PDF magic header");
    }

    #[test]
    fn write_report_pdf_handles_long_input_with_page_break() {
        // Generate enough text to spill onto a second page; assert
        // the PDF is bigger than the single-page baseline.
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let mut long = String::from("# Long report\n\n");
        for i in 0..200 {
            long.push_str(&format!(
                "Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                 Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n\n"
            ));
        }
        let path = ws.write_report_pdf(&id, &long, "Long report").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 5000, "multi-page pdf should be substantially bigger");
    }

    /// 2026-05-04 — header rendering covers h1 through h6 (was:
    /// only h1/h2/h3, with `#### Foo` silently rendering as
    /// h3-with-text-"# Foo" because the `### ` prefix matched
    /// first). Asserts each level returns the right metadata and
    /// that none of them swallow lower-level prefixes.
    #[test]
    fn classify_header_handles_h1_through_h6_in_correct_priority() {
        // h1 > h2 > h3 > h4 > h5 > h6 — sizes strictly decreasing.
        let h1 = classify_header("# Top").unwrap();
        let h2 = classify_header("## Two").unwrap();
        let h3 = classify_header("### Three").unwrap();
        let h4 = classify_header("#### Four").unwrap();
        let h5 = classify_header("##### Five").unwrap();
        let h6 = classify_header("###### Six").unwrap();

        assert_eq!(h1.text, "Top");
        assert_eq!(h2.text, "Two");
        assert_eq!(h3.text, "Three");
        assert_eq!(h4.text, "Four");
        assert_eq!(h5.text, "Five");
        assert_eq!(h6.text, "Six");

        assert!(h1.size_pt > h2.size_pt, "h1 must be larger than h2");
        assert!(h2.size_pt > h3.size_pt);
        assert!(h3.size_pt > h4.size_pt);
        assert!(h4.size_pt >= h5.size_pt); // h4 may equal h5 in pt but pure spacing differs
        assert!(h5.size_pt >= h6.size_pt);

        // Above-gap scales the same way — higher level → more
        // breathing room.
        assert!(h1.above_gap > h2.above_gap);
        assert!(h2.above_gap > h3.above_gap);
        assert!(h3.above_gap > h4.above_gap);
        assert!(h4.above_gap > h5.above_gap);
    }

    #[test]
    fn classify_header_h4_does_not_get_swallowed_by_h3_prefix() {
        // The bug: `#### Foo` matched `### ` first and rendered
        // as h3 with text "# Foo". Catches that regression
        // explicitly.
        let spec = classify_header("#### For Full Sun to Partial Shade").unwrap();
        assert_eq!(spec.text, "For Full Sun to Partial Shade");
        assert!(
            !spec.text.starts_with('#'),
            "h4 prefix must consume all 4 hashes; got {:?}",
            spec.text,
        );
    }

    #[test]
    fn classify_header_returns_none_for_non_heading_lines() {
        assert!(classify_header("plain paragraph").is_none());
        assert!(classify_header("- bullet").is_none());
        assert!(classify_header("").is_none());
        // No-space `#foo` is not a heading.
        assert!(classify_header("#foo").is_none());
    }

    /// 2026-05-04 — friendly PDF filename. Was: hard-coded
    /// "report.pdf" for every research job. Now derived from the
    /// markdown's first H1 (best signal for what the report's
    /// about), slugified to a 3-5 word stub.
    #[test]
    fn derive_report_filename_stem_uses_first_h1_when_present() {
        let md = "# Best Evergreen Ground Covers For 2026\n\nBody here.";
        let stem = derive_report_filename_stem(md, "Research report — abc-123");
        // "the / a / an" stop-words trim from the front; "best" is
        // a content word so it stays.
        assert_eq!(stem, "best-evergreen-ground-covers-for");
    }

    #[test]
    fn derive_report_filename_stem_drops_leading_stopwords() {
        let md = "# The Best Way To Pick A Ground Cover\n";
        let stem = derive_report_filename_stem(md, "fallback");
        // "the" stripped; "best", "way", "to", "pick", "a" → keep
        // first 5 content words but cap at MAX_WORDS.
        assert!(stem.starts_with("best-way-"), "got {stem}");
        assert!(stem.split('-').count() <= 5);
    }

    #[test]
    fn derive_report_filename_stem_falls_back_to_title_param_when_no_h1() {
        let stem = derive_report_filename_stem(
            "Body text only, no headings.",
            "Research report — Recommend evergreen ground covers",
        );
        // Strips the "Research report — " boilerplate.
        assert!(stem.starts_with("recommend-evergreen-"), "got {stem}");
    }

    #[test]
    fn derive_report_filename_stem_hard_fallback_when_everything_empty() {
        let stem = derive_report_filename_stem("", "");
        assert_eq!(stem, "research-report");
    }

    #[test]
    fn write_report_pdf_uses_friendly_filename_with_date() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ResearchWorkspace::new(tmp.path());
        let id = ResearchJobId::new();
        let md = "# Best Evergreen Ground Covers\n\nBody.";
        let path = ws
            .write_report_pdf(&id, md, "Research report — anything")
            .unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("best-evergreen-"), "got {name}");
        // YYYY-MM-DD date suffix before .pdf.
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        assert!(name.contains(&today), "missing date {today}: {name}");
        assert!(name.ends_with(".pdf"));
    }

    #[test]
    fn strip_emphasis_removes_markdown_bold_and_italic_markers() {
        assert_eq!(strip_emphasis("**bold** word"), "bold word");
        assert_eq!(strip_emphasis("__bold__ word"), "bold word");
        assert_eq!(strip_emphasis("an *emphasis* test"), "an emphasis test");
        assert_eq!(strip_emphasis("plain text"), "plain text");
    }
}
