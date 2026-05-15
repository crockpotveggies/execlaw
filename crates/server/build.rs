//! Ensure `web/dist/` exists so `rust-embed` doesn't fail the build
//! on a fresh checkout where the operator hasn't run `npm --prefix
//! web run build` yet. The created directory is empty, so SPA
//! routes serve the friendly "run npm build" diagnostic at runtime
//! instead of compiling to a 200-line cargo error.
//!
//! Also emit `cargo:rerun-if-changed` for the SPA bundle so a
//! `vite build` invalidates the server's incremental compile in
//! release builds (debug builds read from disk per request, so
//! the rerun is purely a release-build correctness guard).

use std::path::PathBuf;

fn main() {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR has at least two parent components")
        .to_path_buf();
    let dist = workspace_root.join("web").join("dist");
    if let Err(e) = std::fs::create_dir_all(&dist) {
        // Don't fail the build — surface the issue and let
        // rust-embed report it precisely if the directory really
        // can't be created. The vast majority of failures here are
        // EROFS / EACCES on locked-down CI volumes; a fallback
        // empty dist directory next to the manifest avoids that.
        eprintln!(
            "warning: could not create {} ({e}); falling back to in-crate dist",
            dist.display()
        );
    }
    println!("cargo:rerun-if-changed={}", dist.display());
}
