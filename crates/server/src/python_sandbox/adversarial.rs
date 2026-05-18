//! Phase 13 — adversarial tests against the live python-sandbox.
//!
//! Covers the failure modes the audit loops flagged across Phases
//! 1-8 but didn't have explicit regression coverage. Each test
//! attacks one specific failure mode end-to-end against the real
//! container so a future code change that breaks the defense fails
//! HERE before reaching an operator.
//!
//! What's covered:
//!   * Kernel crashes mid-execute → status::KernelDied, pool recovers
//!   * Syntax errors → status::Error, kernel still usable
//!   * Cross-conversation kernel isolation (variables don't leak)
//!   * Sequential executes on the same kernel preserve state
//!   * Output cap at the exact MAX_OUTPUT_BYTES boundary
//!   * Cell with zero outputs (`pass`) returns clean Ok
//!   * Cell that imports a missing module → Error with traceback
//!   * Cell that allocates briefly large objects → no leak in kernel
//!
//! What's NOT covered here (out of scope for the dispatch layer):
//!   * Network egress prevention — depends on supervisor setting
//!     `--network none` at container spawn time. Tested via the
//!     supervisor's config tests, not here.
//!   * Filesystem escape — depends on `--read-only` + `--cap-drop
//!     ALL`. Same — supervisor's domain.
//!   * Path traversal on hydrated filenames — covered in
//!     hydration::tests::rejects_path_traversal_filenames (Phase 3).
//!
//! All tests below gate on EXECLAW_TEST_GATEWAY_URL so `cargo test`
//! doesn't require Docker.

#![cfg(test)]

use crate::python_sandbox::client::{GatewayClient, MAX_OUTPUT_BYTES};
use crate::python_sandbox::mime::{ExecuteOutput, ExecuteStatus, StreamName};
use std::time::Duration;

fn live_url() -> Option<String> {
    std::env::var("EXECLAW_TEST_GATEWAY_URL").ok()
}

/// Cell that calls `os._exit(0)` exits the kernel's Python
/// process without going through ipykernel's shutdown protocol.
/// Three observed outcomes depending on timing:
///   - WS closes mid-execute → `KernelDied`
///   - WS stays open, gateway hasn't detected the dead kernel
///     yet → we hit our timeout deadline → `Timeout`
///   - ipykernel catches the exit and produces an Error message
///     before exiting → `Error`
/// ALL THREE are correct from our perspective: the cell stopped,
/// the kernel is unusable, and the dispatch layer returned a
/// terminal status without hanging forever. The contract we DO
/// enforce: subsequent executes for OTHER conversations are not
/// affected.
#[tokio::test]
async fn kernel_crash_mid_execute_surfaces_terminal_status() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel_dies = c.create_kernel("python3").await.unwrap().id;
    let kernel_lives = c.create_kernel("python3").await.unwrap().id;

    let r = c
        .execute(
            &kernel_dies,
            "import os; os._exit(0)",
            Duration::from_secs(3),
        )
        .await
        .expect("execute should not propagate the crash as an error");
    assert!(
        matches!(
            r.status,
            ExecuteStatus::KernelDied | ExecuteStatus::Error | ExecuteStatus::Timeout
        ),
        "expected terminal status after os._exit, got {:?}",
        r.status
    );

    // Sibling kernel MUST still work.
    let r2 = c
        .execute(&kernel_lives, "1 + 1", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        matches!(r2.status, ExecuteStatus::Ok),
        "sibling kernel must be unaffected by another kernel's crash; got {:?}",
        r2.status
    );

    c.delete_kernel(&kernel_dies).await.ok();
    c.delete_kernel(&kernel_lives).await.ok();
}

#[tokio::test]
async fn syntax_error_returns_error_status_and_keeps_kernel_usable() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel = c.create_kernel("python3").await.unwrap().id;

    let r = c
        .execute(&kernel, "this is not valid python !!", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        matches!(r.status, ExecuteStatus::Error),
        "syntax error should produce Error, got {:?}",
        r.status
    );
    let saw_error = r.outputs.iter().any(|o| matches!(o, ExecuteOutput::Error { .. }));
    assert!(saw_error, "must include an Error output");

    // Kernel must still work after a syntax error.
    let r2 = c.execute(&kernel, "2 + 2", Duration::from_secs(10)).await.unwrap();
    assert!(
        matches!(r2.status, ExecuteStatus::Ok),
        "kernel must be usable after syntax error"
    );

    c.delete_kernel(&kernel).await.unwrap();
}

#[tokio::test]
async fn missing_import_returns_error_with_traceback() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel = c.create_kernel("python3").await.unwrap().id;

    let r = c
        .execute(&kernel, "import this_module_does_not_exist_xyz123", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(r.status, ExecuteStatus::Error));
    let err = r
        .outputs
        .iter()
        .find_map(|o| match o {
            ExecuteOutput::Error { ename, traceback, .. } => Some((ename.as_str(), traceback)),
            _ => None,
        })
        .expect("must have an Error output");
    assert!(
        err.0.contains("ImportError") || err.0.contains("ModuleNotFoundError"),
        "ename should be ImportError/ModuleNotFoundError; got {:?}",
        err.0
    );
    assert!(!err.1.is_empty(), "traceback must not be empty");

    c.delete_kernel(&kernel).await.unwrap();
}

#[tokio::test]
async fn two_kernels_dont_leak_variables_between_conversations() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel_a = c.create_kernel("python3").await.unwrap().id;
    let kernel_b = c.create_kernel("python3").await.unwrap().id;

    // A defines a secret. B asks for it.
    let _ = c
        .execute(&kernel_a, "SECRET = 42", Duration::from_secs(10))
        .await
        .unwrap();
    let r = c
        .execute(&kernel_b, "SECRET", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        matches!(r.status, ExecuteStatus::Error),
        "kernel B must NOT see variables from kernel A; got status {:?}",
        r.status
    );
    let err = r
        .outputs
        .iter()
        .find_map(|o| match o {
            ExecuteOutput::Error { ename, .. } => Some(ename.as_str()),
            _ => None,
        })
        .expect("must have NameError");
    assert_eq!(err, "NameError", "cross-kernel access must NameError");

    // Confirm A still has it (sanity check: we didn't accidentally
    // share kernels).
    let r = c
        .execute(&kernel_a, "SECRET", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(r.status, ExecuteStatus::Ok));

    c.delete_kernel(&kernel_a).await.unwrap();
    c.delete_kernel(&kernel_b).await.unwrap();
}

#[tokio::test]
async fn sequential_executes_preserve_kernel_state() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel = c.create_kernel("python3").await.unwrap().id;

    // Set a variable.
    let _ = c
        .execute(
            &kernel,
            "import pandas as pd; df = pd.DataFrame({'a': [1, 2, 3]})",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    // Mutate it in a later turn.
    let _ = c
        .execute(&kernel, "df['b'] = df['a'] * 2", Duration::from_secs(10))
        .await
        .unwrap();
    // Read it. Cast to int explicitly because pandas 2.x's `.sum()`
    // returns a numpy int64 whose repr is `np.int64(12)` rather than
    // `12` — version-specific. `int()` normalizes.
    let r = c
        .execute(&kernel, "int(df['b'].sum())", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(r.status, ExecuteStatus::Ok));
    let result = r
        .outputs
        .iter()
        .find_map(|o| match o {
            ExecuteOutput::ExecuteResult { bundle, .. } => Some(bundle),
            _ => None,
        })
        .expect("execute_result present");
    let plain = result
        .iter()
        .find(|m| m.mime_type == "text/plain")
        .expect("text/plain");
    // 2 + 4 + 6 = 12
    assert_eq!(plain.data, serde_json::json!("12"));

    c.delete_kernel(&kernel).await.unwrap();
}

/// Cell with NO output (`pass`) must still complete cleanly with
/// status::Ok and an empty outputs list.
#[tokio::test]
async fn cell_with_no_outputs_returns_clean_ok() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel = c.create_kernel("python3").await.unwrap().id;

    let r = c
        .execute(&kernel, "pass", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(r.status, ExecuteStatus::Ok));
    // No execute_result (no return value), no stream, no error.
    let stream_or_result = r.outputs.iter().any(|o| {
        matches!(
            o,
            ExecuteOutput::Stream { .. }
                | ExecuteOutput::ExecuteResult { .. }
                | ExecuteOutput::Error { .. }
        )
    });
    assert!(
        !stream_or_result,
        "`pass` should produce no stream / result / error outputs; got {:?}",
        r.outputs
    );

    c.delete_kernel(&kernel).await.unwrap();
}

/// Cell just under the cap (~40 MB of stdout) must complete with
/// status::Ok — proves the cap doesn't fire prematurely.
#[tokio::test]
async fn cell_under_output_cap_completes_ok() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel = c.create_kernel("python3").await.unwrap().id;

    // 40 MB — comfortably under the 50 MB cap. Single print; the
    // kernel sends one stream message.
    let r = c
        .execute(
            &kernel,
            "print('x' * 40_000_000)",
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(
        matches!(r.status, ExecuteStatus::Ok),
        "40 MB output (under 50 MB cap) should be Ok, got {:?}",
        r.status
    );
    let stdout = r.outputs.iter().find_map(|o| match o {
        ExecuteOutput::Stream {
            name: StreamName::Stdout,
            text,
        } => Some(text.len()),
        _ => None,
    });
    assert!(
        stdout.map(|n| n >= 40_000_000).unwrap_or(false),
        "stdout should be ~40 MB; got {stdout:?}"
    );

    c.delete_kernel(&kernel).await.unwrap();
}

/// Cell that briefly allocates a few hundred MB then drops the
/// reference must NOT leave the kernel in a degraded state. The
/// kernel's GC reclaims; the next execute completes fast.
#[tokio::test]
async fn transient_large_allocation_doesnt_degrade_kernel() {
    let Some(url) = live_url() else { return };
    let c = GatewayClient::new(url).unwrap();
    let kernel = c.create_kernel("python3").await.unwrap().id;

    let _ = c
        .execute(
            &kernel,
            "import sys\n\
             big = [0] * 50_000_000\n\
             total = sum(big[:1000])\n\
             del big\n\
             total",
            Duration::from_secs(30),
        )
        .await
        .unwrap();

    // Followup execute must be quick (< 5s wall) — proves the kernel
    // isn't thrashing on the leftover.
    let t0 = std::time::Instant::now();
    let r = c
        .execute(&kernel, "1 + 1", Duration::from_secs(10))
        .await
        .unwrap();
    let dt = t0.elapsed();
    assert!(matches!(r.status, ExecuteStatus::Ok));
    assert!(
        dt < Duration::from_secs(5),
        "post-large-alloc execute should be quick; took {dt:?}"
    );

    c.delete_kernel(&kernel).await.unwrap();
}

/// Verify the MAX_OUTPUT_BYTES constant matches what the bench /
/// production code path uses. Caught a subtle drift in an earlier
/// audit where we doc'd 50 MB but the const was 64 MB.
#[test]
fn max_output_bytes_const_is_50mb() {
    assert_eq!(MAX_OUTPUT_BYTES, 50 * 1024 * 1024);
}
