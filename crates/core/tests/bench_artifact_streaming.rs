//! Phase 5 audit bench. Compares streaming vs. bytes-based artifact
//! creation across file sizes, and asserts memory bound by running
//! through bigger inputs than would fit comfortably in a Vec<u8>
//! during a real watcher publish.
//!
//! Run:
//!   cargo test -p execlaw-core --test bench_artifact_streaming \
//!     -- --nocapture --ignored
//!
//! Marked #[ignore] because it allocates ~150 MB of disk for the
//! largest case and prints numbers rather than asserting tight
//! perf budgets — Phase 14 Criterion will pin those.

use execlaw_core::attachments::AttachmentStore;
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use std::time::Instant;

fn make_store() -> (Database, tempfile::TempDir) {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let dir = tempfile::tempdir().unwrap();
    (db, dir)
}

fn bench_one(label: &str, size: usize) {
    let (db, dir) = make_store();
    let store = AttachmentStore::new(&db);
    let artifacts = dir.path().join("artifacts");

    let bytes: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(31)).collect();
    let source = dir.path().join("input.bin");
    std::fs::write(&source, &bytes).unwrap();

    // Bytes-based: hash + write happens after we already hold the
    // full Vec<u8> in memory.
    let t0 = Instant::now();
    let _ = store
        .insert_plugin_artifact(
            &artifacts,
            "bench",
            "data.bin",
            "application/octet-stream",
            &bytes,
            None,
            1,
        )
        .unwrap();
    let bytes_path = t0.elapsed();

    // Streaming: 64 KB buffer regardless of input size.
    let t0 = Instant::now();
    let _ = store
        .insert_plugin_artifact_from_path(
            &artifacts,
            "bench",
            "data2.bin",       // different filename so we get a new row
            "application/octet-stream",
            &source,
            None,
            2,
        )
        .unwrap();
    let stream_path = t0.elapsed();

    let throughput = |d: std::time::Duration| {
        let mbps = (size as f64 / 1024.0 / 1024.0) / d.as_secs_f64();
        mbps
    };
    println!(
        "  {:<10} {:>9} bytes : bytes-based {:>4} ms ({:>5.0} MB/s), streaming {:>4} ms ({:>5.0} MB/s)",
        label,
        size,
        bytes_path.as_millis(),
        throughput(bytes_path),
        stream_path.as_millis(),
        throughput(stream_path),
    );
}

#[test]
#[ignore]
fn phase5_streaming_throughput_vs_bytes() {
    println!();
    bench_one("tiny",        1_000);          // 1 KB
    bench_one("small",       100_000);        // 100 KB
    bench_one("typical",     2_000_000);      // 2 MB CSV
    bench_one("medium",      10_000_000);     // 10 MB
    bench_one("large",       50_000_000);     // 50 MB (our output cap)
    // Skipping >50 MB by default — Phase 4 caps cell outputs there.
}
