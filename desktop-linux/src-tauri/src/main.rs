#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
fn main() {
    execlaw_tray_linux_lib::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "execlaw-tray (Linux) is Linux-only — use the .app bundle on \
         macOS or the .exe installer on Windows."
    );
    std::process::exit(1);
}
