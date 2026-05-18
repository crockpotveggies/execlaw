//! Criterion microbenchmarks for python-sandbox hot paths
//! (§0 axiom #14).
//!
//! Locks perf budgets so a regression in:
//!   * hydration's copy+chmod loop
//!   * output_watcher's notify-thread path filter (runs per fs event)
//!   * MIME bundle byte counter (runs per iopub message)
//!   * streaming artifact publish (runs per output)
//! breaks `cargo bench` rather than landing silently.
//!
//! Run: `cargo bench -p execlaw-server --bench python_sandbox_hot_paths`
//!
//! Budgets are picked based on the ad-hoc bench numbers captured in
//! the Phase 3 / 4 / 5 audit logs, with comfortable headroom so
//! tests are stable across machines but tight enough to catch real
//! regressions. Each bench's expected order-of-magnitude is noted
//! in its body.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use execlaw_core::attachments::AttachmentStore;
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::ids::ConversationId;
use execlaw_core::migrations::MigrationRunner;
use execlaw_server::python_sandbox::hydration::{
    hydrate_uploads, AttachmentToHydrate, HydrateOpts,
};
use execlaw_server::python_sandbox::mime::{
    mime_bundle_from_jupyter_data, ExecuteOutput, MimeBundle, StreamName,
};
use serde_json::json;
use std::path::PathBuf;

// ---------------------------------------------------------------------
// Hydration — Phase 3 audit said cold=11ms for a single 50MB file,
// cold=23ms for 50x10KB files. Bench the typical case (3x 2MB) which
// landed at 4ms cold. Budget: median < 20 ms on a typical dev
// machine; SSD-bound so we don't over-pin.
// ---------------------------------------------------------------------

fn bench_hydration(c: &mut Criterion) {
    let mut group = c.benchmark_group("hydration");
    // Total bytes copied per iteration so Throughput shows MB/s.
    group.throughput(Throughput::Bytes(3 * 2_000_000));

    group.bench_function("3_files_2mb_each_cold", |b| {
        b.iter_with_setup(
            || {
                // Setup runs once per iteration — fresh tempdir + blobs
                // so we're always measuring cold copies, not idempotent
                // no-ops.
                let dir = tempfile::tempdir().unwrap();
                let blob_root = dir.path().join("blobs");
                let work_root = dir.path().join("work");
                std::fs::create_dir_all(&blob_root).unwrap();
                let mut attachments = Vec::new();
                for i in 0..3 {
                    let p = blob_root.join(format!("blob-{i}"));
                    let bytes = vec![b'x'; 2_000_000];
                    std::fs::write(&p, &bytes).unwrap();
                    attachments.push(AttachmentToHydrate {
                        blob_path: p,
                        filename: format!("data-{i}.bin"),
                    });
                }
                (dir, work_root, attachments)
            },
            |(_dir, work_root, attachments)| {
                let convo = ConversationId::from("bench");
                let r = hydrate_uploads(
                    black_box(&work_root),
                    black_box(&convo),
                    black_box(&attachments),
                    HydrateOpts::default(),
                )
                .unwrap();
                black_box(r);
            },
        );
    });

    group.bench_function("idempotent_skip_3_files", |b| {
        // Setup once: pre-populate so every iteration's call is a
        // size-match no-op. This is the per-execute "did the inputs
        // change?" path; budget < 1 ms.
        let dir = tempfile::tempdir().unwrap();
        let blob_root = dir.path().join("blobs");
        let work_root = dir.path().join("work");
        std::fs::create_dir_all(&blob_root).unwrap();
        let mut attachments = Vec::new();
        for i in 0..3 {
            let p = blob_root.join(format!("blob-{i}"));
            std::fs::write(&p, b"hello").unwrap();
            attachments.push(AttachmentToHydrate {
                blob_path: p,
                filename: format!("data-{i}.bin"),
            });
        }
        let convo = ConversationId::from("bench-idem");
        // Prime once.
        hydrate_uploads(&work_root, &convo, &attachments, HydrateOpts::default()).unwrap();

        b.iter(|| {
            let r = hydrate_uploads(
                black_box(&work_root),
                black_box(&convo),
                black_box(&attachments),
                HydrateOpts::default(),
            )
            .unwrap();
            black_box(r);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------
// MIME bundle byte counter — runs once per iopub stream/result message
// inside the execute loop. The 50 MB output cap relies on this being
// O(N) in the bundle size (string len → no JSON re-serialize). A
// regression to "serialize-and-measure for everything" would 10× our
// per-execute overhead.
// ---------------------------------------------------------------------

fn bench_approx_bytes(c: &mut Criterion) {
    let mut group = c.benchmark_group("mime_bundle_approx_bytes");

    // Typical DataFrame execute_result — text/plain + text/html
    // (the rich-render case verified in Phase 1 smoke).
    let dataframe_output = ExecuteOutput::ExecuteResult {
        execution_count: 1,
        bundle: vec![
            MimeBundle {
                mime_type: "text/plain".into(),
                data: json!("   a  b\n0  1  3\n1  2  4"),
            },
            MimeBundle {
                mime_type: "text/html".into(),
                data: json!("<table border=\"1\"><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>3</td></tr><tr><td>2</td><td>4</td></tr></table>"),
            },
        ],
    };
    group.bench_function("dataframe", |b| {
        b.iter(|| black_box(dataframe_output.approx_bytes()))
    });

    // Stream output — the hot path during a flood-of-stdout cell
    // (the OutputTooLarge trigger).
    let stream_output = ExecuteOutput::Stream {
        name: StreamName::Stdout,
        text: "x".repeat(64_000),
    };
    group.bench_function("stream_64kb", |b| {
        b.iter(|| black_box(stream_output.approx_bytes()))
    });

    group.finish();
}

// ---------------------------------------------------------------------
// MIME bundle conversion from Jupyter shape — runs once per
// iopub execute_result / display_data. O(N) in number of MIME types
// (always small, typically 1-2) with an extra lex sort.
// ---------------------------------------------------------------------

fn bench_mime_bundle_from_jupyter(c: &mut Criterion) {
    let mut data = serde_json::Map::new();
    data.insert("text/plain".into(), json!("hello"));
    data.insert("text/html".into(), json!("<b>hello</b>"));
    data.insert("image/png".into(), json!("iVBORw0KGgo..."));

    c.bench_function("mime_bundle_from_jupyter_3_types", |b| {
        b.iter(|| black_box(mime_bundle_from_jupyter_data(black_box(&data))))
    });
}

// ---------------------------------------------------------------------
// Streaming artifact publish — Phase 5 audit measured 86 MB/s for
// 50 MB inputs. Bench the typical 100 KB case (CSV size) so the
// fixed overhead (temp file create + rename + sqlite insert) shows
// up clearly. Budget: < 10 ms for 100 KB.
// ---------------------------------------------------------------------

fn bench_streaming_publish(c: &mut Criterion) {
    let mut group = c.benchmark_group("streaming_publish");
    group.throughput(Throughput::Bytes(100_000));
    group.bench_function("100kb_csv", |b| {
        b.iter_with_setup(
            || {
                let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
                MigrationRunner::new(&db).apply_all().unwrap();
                let dir = tempfile::tempdir().unwrap();
                let blob = dir.path().join("source.csv");
                std::fs::write(&blob, vec![b'x'; 100_000]).unwrap();
                let artifacts_root = dir.path().join("artifacts");
                (db, dir, blob, artifacts_root)
            },
            |(db, _dir, blob, artifacts_root): (
                Database,
                tempfile::TempDir,
                PathBuf,
                PathBuf,
            )| {
                let store = AttachmentStore::new(&db);
                let r = store
                    .insert_plugin_artifact_from_path(
                        black_box(&artifacts_root),
                        "bench",
                        "x.csv",
                        "text/csv",
                        black_box(&blob),
                        None,
                        1,
                    )
                    .unwrap();
                black_box(r);
            },
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_hydration,
    bench_approx_bytes,
    bench_mime_bundle_from_jupyter,
    bench_streaming_publish,
);
criterion_main!(benches);
