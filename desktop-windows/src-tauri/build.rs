//! Tauri build script — generates context + runs the asset bundler
//! glue. Unlike the macOS sibling crate there's no framework link
//! directive here: Windows service plumbing is reached entirely
//! through the `windows-service` crate, which carries its own link
//! attributes for advapi32.dll.

fn main() {
    tauri_build::build();
}
