//! Ad-hoc Phase 2 audit bench. Times the Rust gateway client's WS
//! execute path against a live container so we can see the
//! steady-state cost without Python script overhead in the way.
//!
//! Run with:
//!   docker run --rm -d --name bench-gw -p 127.0.0.1:18890:8888 \
//!     execlaw/python-sandbox-fast:0.1.0
//!   EXECLAW_TEST_GATEWAY_URL=http://127.0.0.1:18890 \
//!     cargo test -p execlaw-server --lib \
//!     python_sandbox::bench_phase2 -- --nocapture --ignored
//!   docker rm -f bench-gw
//!
//! Marked #[ignore] because it requires a live gateway, takes a few
//! seconds, and prints numbers rather than asserting — the Criterion
//! bench suite in Phase 14 is where we'll lock perf budgets.

#![cfg(test)]

use super::client::GatewayClient;
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore]
async fn phase2_execute_latency_bench() {
    let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
        eprintln!(
            "skipping: set EXECLAW_TEST_GATEWAY_URL=http://127.0.0.1:18890 \
             and `docker run -p 127.0.0.1:18890:8888 ...` first"
        );
        return;
    };
    let c = GatewayClient::new(url).expect("client");
    let kernel = c.create_kernel("python3").await.expect("spawn").id;

    // Warmup so jit-y things settle (Python kernel side mostly).
    for _ in 0..2 {
        let _ = c
            .execute(&kernel, "1 + 1", Duration::from_secs(10))
            .await
            .expect("warmup");
    }

    // Cold WS connect — each execute opens a fresh WS, so this IS the
    // per-execute steady-state cost we'll see in production.
    const N: u32 = 20;
    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;
    let mut best = Duration::from_secs(999);
    for _ in 0..N {
        let t0 = Instant::now();
        let _ = c
            .execute(&kernel, "1 + 1", Duration::from_secs(10))
            .await
            .expect("execute");
        let dt = t0.elapsed();
        total += dt;
        if dt > worst {
            worst = dt;
        }
        if dt < best {
            best = dt;
        }
    }
    let avg = total / N;
    println!(
        "\nphase 2 WS execute (1+1, warm kernel, N={N}):\n  best  {:>5} ms\n  avg   {:>5} ms\n  worst {:>5} ms",
        best.as_millis(),
        avg.as_millis(),
        worst.as_millis()
    );

    // Per-output cost on a DataFrame execute (richer MIME bundle).
    const M: u32 = 10;
    let mut total = Duration::ZERO;
    for _ in 0..M {
        let t0 = Instant::now();
        let _ = c
            .execute(
                &kernel,
                "import pandas as pd; pd.DataFrame({'a':range(100),'b':range(100)})",
                Duration::from_secs(10),
            )
            .await
            .expect("DataFrame execute");
        total += t0.elapsed();
    }
    let avg = total / M;
    println!(
        "phase 2 WS execute (100-row DataFrame, warm, N={M}):\n  avg   {:>5} ms",
        avg.as_millis()
    );

    c.delete_kernel(&kernel).await.expect("cleanup");
}
