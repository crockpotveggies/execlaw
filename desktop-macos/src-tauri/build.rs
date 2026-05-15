//! Tauri build script — generates context, runs the asset bundler
//! glue, and (on macOS) links the ServiceManagement framework so
//! the `SMAppService` ObjC class symbols resolve at link time.

fn main() {
    #[cfg(target_os = "macos")]
    {
        // The ServiceManagement framework hosts SMAppService and
        // related classes added in macOS 13. Without this directive
        // `+[SMAppService agentServiceWithPlistName:]` resolves to
        // a nil class at runtime (because objc_lookupClass returns
        // nil for unlinked frameworks) and `register()` silently
        // fails with "no such service".
        println!("cargo:rustc-link-lib=framework=ServiceManagement");
    }
    tauri_build::build();
}
