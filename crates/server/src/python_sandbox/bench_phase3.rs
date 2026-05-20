//! Ad-hoc Phase 3 audit bench. Times hydration across a range of
//! attachment sizes so we know the latency budget before wiring
//! into the supervisor's spawn path.
//!
//! Run with:
//!   cargo test -p execlaw-server --lib \
//!     python_sandbox::bench_phase3 -- --nocapture --ignored
//!
//! Marked #[ignore] because it produces files in a tempdir and
//! prints numbers rather than asserting.

#![cfg(test)]

use super::hydration::{AttachmentToHydrate, HydrateOpts, hydrate_uploads};
use execlaw_core::ids::ConversationId;
use std::fs;
use std::time::Instant;

fn bench_size(label: &str, blob_size: usize, n: usize) {
    let dir = tempfile::tempdir().unwrap();
    let blob_root = dir.path().join("blobs");
    let work_root = dir.path().join("work");
    fs::create_dir_all(&blob_root).unwrap();
    fs::create_dir_all(&work_root).unwrap();
    let mut attachments = Vec::new();
    for i in 0..n {
        let bytes = vec![b'x'; blob_size];
        let p = blob_root.join(format!("blob-{i}"));
        fs::write(&p, &bytes).unwrap();
        attachments.push(AttachmentToHydrate {
            blob_path: p,
            filename: format!("file-{i}.bin"),
        });
    }
    let convo = ConversationId::from(format!("bench-{label}"));
    let t0 = Instant::now();
    hydrate_uploads(&work_root, &convo, &attachments, HydrateOpts::default()).unwrap();
    let cold = t0.elapsed();
    let t1 = Instant::now();
    hydrate_uploads(&work_root, &convo, &attachments, HydrateOpts::default()).unwrap();
    let warm = t1.elapsed();
    println!(
        "  {:<18} {:>3} files x {:>9} bytes: cold {:>5} ms, warm {:>3} ms",
        label,
        n,
        blob_size,
        cold.as_millis(),
        warm.as_millis()
    );
}

#[test]
#[ignore]
fn phase3_hydration_latency_bench() {
    println!();
    bench_size("tiny", 1_000, 5); // 5 small text files
    bench_size("small_csv", 100_000, 3); // 3x 100KB CSVs
    bench_size("typical", 2_000_000, 3); // 3x 2MB CSV (a "small data" scenario)
    bench_size("large_csv", 50_000_000, 1); // single 50MB CSV (analyst common-case ceiling)
    bench_size("many", 10_000, 50); // 50 small files (chat with lots of attachments)
}
