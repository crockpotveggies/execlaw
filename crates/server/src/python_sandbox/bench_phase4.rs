//! Phase 4 audit bench. Two angles:
//!
//! 1. End-to-end latency: how soon after a file is fully written
//!    does the callback fire? Lower bound = debounce window;
//!    interesting upper bound is debounce + timer tick.
//!
//! 2. Burst handling: a single execute writing many distinct
//!    output files (think `df.groupby('region').apply(...).to_csv()`
//!    one CSV per group) must coalesce per-file, fire all callbacks,
//!    not lose any.

#![cfg(test)]

use super::output_watcher::{DEFAULT_DEBOUNCE, OutputWatcher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore]
async fn phase4_debounce_to_callback_latency() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (tx, rx) = mpsc::channel::<Instant>();
    let debounce = DEFAULT_DEBOUNCE; // 300 ms
    let _w = OutputWatcher::start(root.clone(), debounce, move |_e| {
        let _ = tx.send(Instant::now());
    })
    .unwrap();
    // Let recursive watch register.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let outputs = root.join("c-1").join("outputs");
    std::fs::create_dir_all(&outputs).unwrap();

    const N: u32 = 10;
    let mut deltas: Vec<u128> = Vec::new();
    for i in 0..N {
        let p = outputs.join(format!("file-{i}.txt"));
        let write_at = Instant::now();
        std::fs::write(&p, b"hello").unwrap();

        let fired_at = loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Ok(t) = rx.try_recv() {
                break t;
            }
        };
        let total = (fired_at - write_at).as_millis();
        let beyond_debounce = total.saturating_sub(debounce.as_millis());
        deltas.push(beyond_debounce);
    }

    deltas.sort();
    let med = deltas[deltas.len() / 2];
    let max = *deltas.last().unwrap();
    println!(
        "\nphase 4 debounce->callback latency (300ms debounce, N={N}):\n\
           overhead beyond debounce  median {med} ms, max {max} ms"
    );
    // Sanity: median overhead should be well under one tick interval (100ms).
    assert!(med < 200, "median overhead too high: {med} ms");
}

#[tokio::test]
#[ignore]
async fn phase4_burst_no_drops() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let count = Arc::new(AtomicU32::new(0));
    let count_for_cb = count.clone();
    let _w = OutputWatcher::start(root.clone(), Duration::from_millis(200), move |_e| {
        count_for_cb.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let outputs = root.join("c-1").join("outputs");
    std::fs::create_dir_all(&outputs).unwrap();

    const N: u32 = 200;
    let t0 = Instant::now();
    for i in 0..N {
        std::fs::write(outputs.join(format!("f-{i:04}.txt")), b"x").unwrap();
    }
    let burst_time = t0.elapsed();

    // Wait long enough for debounce + a safety margin on the timer ticks.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let fired = count.load(Ordering::SeqCst);
    println!(
        "\nphase 4 burst: wrote {N} files in {:?}, callback fired {fired} times",
        burst_time
    );
    assert_eq!(
        fired, N,
        "every distinct file must produce exactly one callback"
    );
}

#[tokio::test]
#[ignore]
async fn phase4_chunked_write_coalesces_to_one_callback() {
    // Simulate the kernel writing a 1 MB Parquet file via many
    // ~32 KB write() calls — each chunk-write fires notify Modify.
    // The watcher MUST collapse the burst to one callback.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let count = Arc::new(AtomicU32::new(0));
    let count_for_cb = count.clone();
    let _w = OutputWatcher::start(root.clone(), Duration::from_millis(300), move |_e| {
        count_for_cb.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let outputs = root.join("c-1").join("outputs");
    std::fs::create_dir_all(&outputs).unwrap();
    let target = outputs.join("chunked.parquet");

    use std::io::Write;
    let mut f = std::fs::File::create(&target).unwrap();
    let chunk = vec![b'x'; 32 * 1024];
    for _ in 0..32 {
        f.write_all(&chunk).unwrap();
        // Brief pause so each chunk-write is its own filesystem event.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(f);

    // Wait debounce + margin.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let fired = count.load(Ordering::SeqCst);
    println!("\nphase 4 chunked write (32x 32KB writes, ~5ms apart): callback fired {fired} times");
    assert_eq!(fired, 1, "chunked writes must coalesce to one callback");
}
