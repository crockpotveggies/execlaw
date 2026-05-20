// Suppress the extra console window that would otherwise appear on
// Windows release builds. Debug builds keep stdout so `cargo run`
// shows tracing output without redirection.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
fn main() {
    execlaw_tray_win_lib::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "execlaw-tray (Windows) is Windows-only — use `execlaw service install` \
         on Linux, or the .app bundle on macOS."
    );
    std::process::exit(1);
}
