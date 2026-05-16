// Prevents additional console window on Windows (irrelevant for a
// macOS-only crate, but the lint complains otherwise).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
fn main() {
    execlaw_tray_lib::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("execlaw-tray is macOS-only — use `execlaw service install` on Linux/Windows.");
    std::process::exit(1);
}
