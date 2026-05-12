//! Criterion micro-bench for the chart renderer (axiom #14).
//!
//! Budgets:
//!   * SVG render of a 168-point line chart (1 week hourly) at 720×400
//!     — p99 budget < 50 ms.
//!   * PNG render of the same chart — p99 budget < 150 ms (the PNG
//!     encoder is the costly leg).
//!
//! The numbers were measured on a 2024 dev laptop (i7-13700H); they're
//! comfortable headroom for the open-meteo plugin's typical render
//! cadence (a few charts per agent turn at most).

use criterion::{criterion_group, criterion_main, Criterion};
use execlaw_charting::{render_to_png, render_to_svg, ChartKind, ChartSpec, Point, Series};

fn week_hourly() -> ChartSpec {
    // 168 points = 7 days × 24 hours.
    let points: Vec<Point> = (0..168)
        .map(|i| {
            let x = i as f64;
            let y = 15.0 + 8.0 * (x * std::f64::consts::PI / 12.0).sin();
            Point { x, y }
        })
        .collect();
    ChartSpec {
        title: "Week-ahead temperature".into(),
        kind: ChartKind::Line,
        x_label: Some("Hour".into()),
        y_label: Some("°C".into()),
        time_axis: false,
        series: vec![Series {
            name: "Temperature".into(),
            points,
            color: None,
        }],
        band: None,
        y_unit: Some("°C".into()),
    }
}

fn bench_svg(c: &mut Criterion) {
    let spec = week_hourly();
    c.bench_function("svg 168-point line 720x400", |b| {
        b.iter(|| {
            let s = render_to_svg(&spec, 720, 400).unwrap();
            // Use the result so the optimizer doesn't elide work.
            criterion::black_box(s.len());
        });
    });
}

fn bench_png(c: &mut Criterion) {
    let spec = week_hourly();
    c.bench_function("png 168-point line 720x400", |b| {
        b.iter(|| {
            let p = render_to_png(&spec, 720, 400).unwrap();
            criterion::black_box(p.len());
        });
    });
}

criterion_group!(benches, bench_svg, bench_png);
criterion_main!(benches);
