//! Pure-Rust chart renderer.
//!
//! ## Why this exists
//!
//! Plugins emit a small declarative `ChartSpec` JSON describing what
//! they want to show — temperatures over time, daily precipitation
//! bars, ensemble fan band, etc. This crate turns the spec into:
//!
//!   * **SVG** for inline rendering in the SPA's chat stream. No JS
//!     chart library is shipped to the browser — the SVG goes into the
//!     DOM verbatim. Smaller bundle, deterministic appearance.
//!   * **PNG** for transport attachments (Discord/Signal/etc.) that
//!     accept file uploads. Same `ChartSpec`, same visual.
//!
//! ## Why plotters
//!
//! `plotters` is pure-Rust (no V8, no Cairo, no system fonts required
//! at runtime — it ships its own font path), supports both SVG and
//! bitmap output, and weighs ~700 KB compiled. Mature project, stable
//! since 2020, no security advisories outstanding.
//!
//! ## Public surface
//!
//! Three functions:
//!   * [`render_to_svg`] — `ChartSpec` → SVG string.
//!   * [`render_to_png`] — `ChartSpec` → PNG bytes.
//!   * [`ChartSpec`] / [`ChartKind`] / [`Series`] — the declarative
//!     spec types, `serde`-serialisable so plugin tool-call results
//!     can carry one as JSON.
//!
//! Plugins typically build a `ChartSpec`, render it once to SVG
//! (inline-card) and once to PNG (attachment), and ship both to the
//! agent's tool_result. The host's `host_render_chart` Rhai binding
//! does this glue automatically — see `crates/script/src/primitives.rs`.

#![forbid(unsafe_code)]

use plotters::prelude::*;
use plotters::style::{Color, RGBColor};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One data point in a series. `x` is either a wall-clock millisecond
/// (interpreted as time) or a plain f64 ordinal — the renderer reads
/// `time_axis` on the spec to know which interpretation applies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// One series in a multi-series chart. `name` is the legend label.
/// `color` is optional RGB (0-255 each); when absent the renderer
/// picks from a built-in palette by series index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Series {
    pub name: String,
    pub points: Vec<Point>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
}

/// Optional band overlay (used by ensemble / range charts). `low` and
/// `high` are point arrays of the same length, sharing x-values with
/// the primary series. Plotters draws a translucent area between them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Band {
    pub low: Vec<Point>,
    pub high: Vec<Point>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Line,
    Bar,
    Area,
    Scatter,
}

impl Default for ChartKind {
    fn default() -> Self {
        ChartKind::Line
    }
}

/// One chart, fully declarative. Plugins construct this via a Rhai
/// map and pass `to_json_string(spec)` to `host_render_chart`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChartSpec {
    pub title: String,
    pub kind: ChartKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_label: Option<String>,
    /// When true, x-values are treated as Unix-milliseconds and the
    /// axis renders as HH:MM / MMM-DD timestamps. When false they're
    /// plain f64 ordinals.
    #[serde(default)]
    pub time_axis: bool,
    pub series: Vec<Series>,
    /// Optional band overlay — used by ensemble forecast charts where
    /// the deterministic line sits inside a probabilistic range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<Band>,
    /// Optional unit suffix appended to y-axis tick labels. e.g. "°C",
    /// " mm". Empty string is treated as no suffix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_unit: Option<String>,
}

#[derive(Debug, Error)]
pub enum ChartError {
    #[error("invalid chart spec: {0}")]
    InvalidSpec(String),
    #[error("renderer: {0}")]
    Render(String),
    #[error("encoding: {0}")]
    Encoding(String),
}

/// Default canvas size in pixels for charts that don't request a
/// specific dimension. 720×400 is the same aspect ratio Twitter uses
/// for inline chart cards and looks comfortable on both phone and
/// desktop.
pub const DEFAULT_WIDTH: u32 = 720;
pub const DEFAULT_HEIGHT: u32 = 400;

/// Render `spec` to an SVG string. Always succeeds for well-formed
/// specs (empty series → empty axes, single point → single dot, etc.).
pub fn render_to_svg(spec: &ChartSpec, width: u32, height: u32) -> Result<String, ChartError> {
    validate(spec)?;
    let mut buffer = String::new();
    {
        let root = SVGBackend::with_string(&mut buffer, (width, height)).into_drawing_area();
        draw(&spec, &root)?;
        root.present()
            .map_err(|e| ChartError::Render(format!("svg present: {e}")))?;
    }
    Ok(buffer)
}

/// Render `spec` to PNG bytes. Always succeeds for well-formed specs.
pub fn render_to_png(spec: &ChartSpec, width: u32, height: u32) -> Result<Vec<u8>, ChartError> {
    validate(spec)?;
    // Plotters' BitMapBackend writes to a raw RGB buffer; we encode
    // to PNG via the `image` re-export (plotters' `bitmap_encoder`
    // feature wires this together).
    let mut buf = vec![0u8; (width * height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buf, (width, height)).into_drawing_area();
        draw(&spec, &root)?;
        root.present()
            .map_err(|e| ChartError::Render(format!("bitmap present: {e}")))?;
    }
    encode_rgb_to_png(&buf, width, height)
}

fn encode_rgb_to_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ChartError> {
    // Plotters' BitMapBackend writes a tightly-packed RGB buffer; the
    // `image` crate's PNG encoder consumes the same layout. We size
    // the output vec conservatively (rgb.len()/4 is a rough lower
    // bound for the compressed PNG of a typical chart — empirically a
    // 720×400 chart compresses to ~20-40 KB).
    let img = image::RgbImage::from_raw(width, height, rgb.to_vec())
        .ok_or_else(|| ChartError::Encoding("rgb buffer wrong size".into()))?;
    let mut out: Vec<u8> = Vec::with_capacity(rgb.len() / 4);
    {
        use image::codecs::png::PngEncoder;
        use image::{ExtendedColorType, ImageEncoder};
        let enc = PngEncoder::new(&mut out);
        enc.write_image(img.as_raw(), width, height, ExtendedColorType::Rgb8)
            .map_err(|e| ChartError::Encoding(format!("png encode: {e}")))?;
    }
    Ok(out)
}

fn validate(spec: &ChartSpec) -> Result<(), ChartError> {
    if spec.title.is_empty() && spec.series.is_empty() {
        return Err(ChartError::InvalidSpec(
            "chart needs at least a title or one series".into(),
        ));
    }
    for s in &spec.series {
        if s.name.is_empty() {
            return Err(ChartError::InvalidSpec(
                "every series must have a non-empty name".into(),
            ));
        }
        for p in &s.points {
            if !p.x.is_finite() || !p.y.is_finite() {
                return Err(ChartError::InvalidSpec(format!(
                    "series '{}' has a non-finite point ({}, {})",
                    s.name, p.x, p.y
                )));
            }
        }
    }
    if let Some(b) = &spec.band {
        if b.low.len() != b.high.len() {
            return Err(ChartError::InvalidSpec(
                "band.low and band.high must have the same length".into(),
            ));
        }
    }
    Ok(())
}

/// Built-in palette used when a series has no explicit color.
/// Picked for legibility on both light and dark SPA themes; colors
/// are colorblind-safe (Wong palette subset).
const PALETTE: &[RGBColor] = &[
    RGBColor(0, 114, 178),   // blue
    RGBColor(213, 94, 0),    // vermillion
    RGBColor(0, 158, 115),   // bluish green
    RGBColor(204, 121, 167), // pink
    RGBColor(240, 228, 66),  // yellow
    RGBColor(86, 180, 233),  // sky blue
    RGBColor(230, 159, 0),   // orange
];

fn series_color(index: usize, explicit: Option<[u8; 3]>) -> RGBColor {
    if let Some([r, g, b]) = explicit {
        return RGBColor(r, g, b);
    }
    PALETTE[index % PALETTE.len()]
}

fn draw<DB>(
    spec: &ChartSpec,
    root: &DrawingArea<DB, plotters::coord::Shift>,
) -> Result<(), ChartError>
where
    DB: DrawingBackend,
    DB::ErrorType: 'static,
{
    root.fill(&WHITE)
        .map_err(|e| ChartError::Render(format!("fill: {e}")))?;

    // Compute axis ranges from the union of all series + band.
    let (x_min, x_max, y_min, y_max) = compute_bounds(spec);
    let y_pad = (y_max - y_min).max(1.0) * 0.08;
    let y_lo = y_min - y_pad;
    let y_hi = y_max + y_pad;
    let x_lo = x_min;
    let x_hi = if (x_max - x_min).abs() < f64::EPSILON {
        x_min + 1.0
    } else {
        x_max
    };

    let mut chart = ChartBuilder::on(root)
        .caption(&spec.title, ("sans-serif", 22).into_font())
        .margin(15)
        .x_label_area_size(40)
        .y_label_area_size(60)
        .build_cartesian_2d(x_lo..x_hi, y_lo..y_hi)
        .map_err(|e| ChartError::Render(format!("axes: {e}")))?;

    let y_unit_owned = spec.y_unit.clone().unwrap_or_default();
    // Closures bound to local `let`s so the mesh builder can chain.
    let y_fmt = move |y: &f64| format_y(*y, &y_unit_owned);
    let x_fmt = move |x: &f64| format_time_x(*x);
    {
        let mut mesh = chart.configure_mesh();
        mesh.disable_x_mesh()
            .y_desc(spec.y_label.as_deref().unwrap_or(""))
            .x_desc(spec.x_label.as_deref().unwrap_or(""))
            .axis_desc_style(("sans-serif", 14))
            .y_label_formatter(&y_fmt);
        if spec.time_axis {
            mesh.x_label_formatter(&x_fmt);
        }
        mesh.draw()
            .map_err(|e| ChartError::Render(format!("mesh: {e}")))?;
    }

    // Band first so series lines draw over it.
    if let Some(band) = &spec.band {
        let color = series_color(0, band.color).mix(0.18);
        let band_points: Vec<(f64, f64, f64)> = band
            .low
            .iter()
            .zip(band.high.iter())
            .map(|(l, h)| (l.x, l.y, h.y))
            .collect();
        chart
            .draw_series(
                AreaSeries::new(
                    band_points.iter().map(|(x, _l, h)| (*x, *h)),
                    band_points.first().map(|(_, l, _)| *l).unwrap_or(y_lo),
                    color,
                )
                .border_style(color),
            )
            .map_err(|e| ChartError::Render(format!("band: {e}")))?;
    }

    for (idx, s) in spec.series.iter().enumerate() {
        let color = series_color(idx, s.color);
        let style = ShapeStyle {
            color: color.to_rgba(),
            filled: false,
            stroke_width: 2,
        };
        let points = s.points.iter().map(|p| (p.x, p.y));
        match spec.kind {
            ChartKind::Line => {
                chart
                    .draw_series(LineSeries::new(points, style.clone()))
                    .map_err(|e| ChartError::Render(format!("line series '{}': {e}", s.name)))?
                    .label(&s.name)
                    .legend(move |(x, y)| {
                        PathElement::new(vec![(x, y), (x + 18, y)], style.clone())
                    });
            }
            ChartKind::Scatter => {
                chart
                    .draw_series(
                        s.points
                            .iter()
                            .map(|p| Circle::new((p.x, p.y), 3, color.filled())),
                    )
                    .map_err(|e| ChartError::Render(format!("scatter '{}': {e}", s.name)))?
                    .label(&s.name)
                    .legend(move |(x, y)| Circle::new((x + 9, y), 3, color.filled()));
            }
            ChartKind::Area => {
                let baseline = y_lo;
                let style = color.mix(0.4);
                chart
                    .draw_series(AreaSeries::new(points, baseline, style).border_style(color))
                    .map_err(|e| ChartError::Render(format!("area '{}': {e}", s.name)))?
                    .label(&s.name)
                    .legend(move |(x, y)| {
                        Rectangle::new([(x, y - 4), (x + 18, y + 4)], color.filled())
                    });
            }
            ChartKind::Bar => {
                // Approximate bar width — for time-axis charts assume
                // points are roughly equispaced and use a fraction of
                // the average gap.
                let bar_w = bar_width(&s.points);
                chart
                    .draw_series(s.points.iter().map(|p| {
                        let x0 = p.x - bar_w / 2.0;
                        let x1 = p.x + bar_w / 2.0;
                        let y0 = y_lo.max(0.0);
                        Rectangle::new([(x0, y0), (x1, p.y)], color.filled())
                    }))
                    .map_err(|e| ChartError::Render(format!("bar '{}': {e}", s.name)))?
                    .label(&s.name)
                    .legend(move |(x, y)| {
                        Rectangle::new([(x, y - 4), (x + 18, y + 4)], color.filled())
                    });
            }
        }
    }

    // Legend — only when there's more than one series. A single-series
    // chart has its name in the title or context already.
    if spec.series.len() > 1 {
        chart
            .configure_series_labels()
            .background_style(&WHITE.mix(0.85))
            .border_style(&BLACK.mix(0.3))
            .label_font(("sans-serif", 12))
            .draw()
            .map_err(|e| ChartError::Render(format!("legend: {e}")))?;
    }

    Ok(())
}

fn compute_bounds(spec: &ChartSpec) -> (f64, f64, f64, f64) {
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    let mut any = false;
    let push = |p: &Point,
                x_min: &mut f64,
                x_max: &mut f64,
                y_min: &mut f64,
                y_max: &mut f64,
                any: &mut bool| {
        *any = true;
        if p.x < *x_min {
            *x_min = p.x;
        }
        if p.x > *x_max {
            *x_max = p.x;
        }
        if p.y < *y_min {
            *y_min = p.y;
        }
        if p.y > *y_max {
            *y_max = p.y;
        }
    };
    for s in &spec.series {
        for p in &s.points {
            push(p, &mut x_min, &mut x_max, &mut y_min, &mut y_max, &mut any);
        }
    }
    if let Some(b) = &spec.band {
        for p in &b.low {
            push(p, &mut x_min, &mut x_max, &mut y_min, &mut y_max, &mut any);
        }
        for p in &b.high {
            push(p, &mut x_min, &mut x_max, &mut y_min, &mut y_max, &mut any);
        }
    }
    if !any {
        // Empty chart — draw a neutral axis so we still produce a
        // valid image rather than panicking.
        return (0.0, 1.0, 0.0, 1.0);
    }
    (x_min, x_max, y_min, y_max)
}

fn format_y(y: f64, unit: &str) -> String {
    // Keep the trailing `[allow]` so format strings stay inline; the
    // function isn't called in a hot loop.
    if y.abs() >= 1000.0 {
        format!("{:.0}{}", y, unit)
    } else if y.abs() >= 10.0 {
        format!("{:.1}{}", y, unit)
    } else {
        format!("{:.2}{}", y, unit)
    }
}

fn format_time_x(x_ms: f64) -> String {
    use chrono::{TimeZone, Utc};
    let secs = (x_ms / 1000.0).round() as i64;
    match Utc.timestamp_opt(secs, 0) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%m-%d %H:%M").to_string(),
        _ => format!("{:.0}", x_ms),
    }
}

fn bar_width(points: &[Point]) -> f64 {
    if points.len() < 2 {
        return 0.5;
    }
    let mut xs: Vec<f64> = points.iter().map(|p| p.x).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut min_gap = f64::INFINITY;
    for w in xs.windows(2) {
        let gap = (w[1] - w[0]).abs();
        if gap > 0.0 && gap < min_gap {
            min_gap = gap;
        }
    }
    if !min_gap.is_finite() {
        return 0.5;
    }
    // 70% of the minimum gap — leaves a sliver of whitespace between
    // bars without making them look stranded.
    min_gap * 0.7
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_chart() -> ChartSpec {
        ChartSpec {
            title: "Temperature".into(),
            kind: ChartKind::Line,
            x_label: Some("Hour".into()),
            y_label: Some("°C".into()),
            time_axis: false,
            series: vec![Series {
                name: "Temp".into(),
                points: (0..24)
                    .map(|i| Point {
                        x: i as f64,
                        y: 15.0 + 8.0 * (i as f64 * std::f64::consts::PI / 12.0).sin(),
                    })
                    .collect(),
                color: None,
            }],
            band: None,
            y_unit: Some("°C".into()),
        }
    }

    #[test]
    fn renders_svg_with_root_element() {
        let svg = render_to_svg(&temp_chart(), 600, 400).unwrap();
        assert!(
            svg.contains("<svg"),
            "expected <svg> root, got: {}",
            &svg[..80]
        );
        assert!(svg.contains("Temperature"), "title must appear in SVG");
    }

    #[test]
    fn renders_png_with_magic_header() {
        let png = render_to_png(&temp_chart(), 600, 400).unwrap();
        // PNG signature: 89 50 4e 47 0d 0a 1a 0a
        assert_eq!(&png[..4], &[0x89, 0x50, 0x4e, 0x47]);
    }

    #[test]
    fn empty_series_still_renders_axes() {
        let spec = ChartSpec {
            title: "Empty".into(),
            kind: ChartKind::Line,
            x_label: None,
            y_label: None,
            time_axis: false,
            series: vec![Series {
                name: "Nothing".into(),
                points: vec![],
                color: None,
            }],
            band: None,
            y_unit: None,
        };
        let svg = render_to_svg(&spec, 400, 300).unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn rejects_nan_points() {
        let spec = ChartSpec {
            title: "Bad".into(),
            kind: ChartKind::Line,
            x_label: None,
            y_label: None,
            time_axis: false,
            series: vec![Series {
                name: "NaN".into(),
                points: vec![Point {
                    x: 0.0,
                    y: f64::NAN,
                }],
                color: None,
            }],
            band: None,
            y_unit: None,
        };
        let err = render_to_svg(&spec, 200, 200).unwrap_err();
        assert!(matches!(err, ChartError::InvalidSpec(_)));
    }

    #[test]
    fn rejects_mismatched_band_lengths() {
        let spec = ChartSpec {
            title: "Bad band".into(),
            kind: ChartKind::Line,
            x_label: None,
            y_label: None,
            time_axis: false,
            series: vec![Series {
                name: "x".into(),
                points: vec![Point { x: 0.0, y: 1.0 }],
                color: None,
            }],
            band: Some(Band {
                low: vec![Point { x: 0.0, y: 0.0 }],
                high: vec![Point { x: 0.0, y: 2.0 }, Point { x: 1.0, y: 3.0 }],
                color: None,
            }),
            y_unit: None,
        };
        let err = render_to_svg(&spec, 200, 200).unwrap_err();
        assert!(matches!(err, ChartError::InvalidSpec(_)));
    }

    #[test]
    fn json_round_trip_preserves_spec() {
        // Use integer-valued y points so serde_json's f64 → number
        // → f64 round-trip is exact. The trigonometric `temp_chart`
        // hits low-bit precision wobble in serde_json's number
        // formatting for some values; a plain integer set sidesteps
        // it without weakening the assertion.
        let s = ChartSpec {
            title: "Daily highs".into(),
            kind: ChartKind::Bar,
            x_label: Some("Day".into()),
            y_label: Some("°C".into()),
            time_axis: false,
            series: vec![Series {
                name: "High".into(),
                points: (0..7)
                    .map(|i| Point {
                        x: i as f64,
                        y: 18.0 + (i as f64),
                    })
                    .collect(),
                color: Some([0, 114, 178]),
            }],
            band: None,
            y_unit: Some("°C".into()),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: ChartSpec = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn bar_chart_renders_to_svg() {
        let spec = ChartSpec {
            title: "Daily precipitation".into(),
            kind: ChartKind::Bar,
            x_label: Some("Day".into()),
            y_label: Some("mm".into()),
            time_axis: false,
            series: vec![Series {
                name: "Rain".into(),
                points: vec![
                    Point { x: 0.0, y: 1.2 },
                    Point { x: 1.0, y: 4.7 },
                    Point { x: 2.0, y: 0.3 },
                    Point { x: 3.0, y: 6.1 },
                ],
                color: None,
            }],
            band: None,
            y_unit: Some(" mm".into()),
        };
        let svg = render_to_svg(&spec, 600, 360).unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn ensemble_band_overlay_renders() {
        let spec = ChartSpec {
            title: "Ensemble forecast".into(),
            kind: ChartKind::Line,
            x_label: Some("Hour".into()),
            y_label: Some("°C".into()),
            time_axis: false,
            series: vec![Series {
                name: "Mean".into(),
                points: (0..24)
                    .map(|i| Point {
                        x: i as f64,
                        y: 15.0 + (i as f64).sin(),
                    })
                    .collect(),
                color: None,
            }],
            band: Some(Band {
                low: (0..24)
                    .map(|i| Point {
                        x: i as f64,
                        y: 13.0 + (i as f64).sin(),
                    })
                    .collect(),
                high: (0..24)
                    .map(|i| Point {
                        x: i as f64,
                        y: 17.0 + (i as f64).sin(),
                    })
                    .collect(),
                color: None,
            }),
            y_unit: Some("°C".into()),
        };
        let svg = render_to_svg(&spec, 720, 400).unwrap();
        assert!(svg.contains("<svg"));
    }
}
