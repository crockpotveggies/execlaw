//! Tauri build script — generates context + runs the asset bundler
//! glue. Linux uses webkit2gtk + libayatana-appindicator at the
//! system-library layer; both are surfaced through Tauri's own
//! `tauri-build` crate via pkg-config probes (the .deb manifest in
//! `tauri.conf.json` declares the runtime deps explicitly so apt
//! refuses to install on a host that's missing them).

fn main() {
    tauri_build::build();
}
