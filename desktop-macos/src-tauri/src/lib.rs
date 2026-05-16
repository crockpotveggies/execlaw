//! execlaw menu bar app — public entry point.
//!
//! `main.rs` calls `run()`. Every module is gated behind
//! `cfg(target_os = "macos")` because the bindings (`SMAppService`,
//! `NSApplication.setActivationPolicy:`) only exist on macOS. The
//! Cargo manifest already restricts this crate to macOS at the
//! workspace level, but the cfg gate gives a nicer error than a
//! mid-build linker failure when somebody tries.

#![cfg(target_os = "macos")]

mod app;
mod server_status;
mod sm_app_service;

pub fn run() {
    // `EXECLAW_TRAY_LOG` overrides the default; otherwise we emit
    // INFO and above so first-launch issues show up without
    // requiring the operator to flip a flag.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("EXECLAW_TRAY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    app::run();
}
