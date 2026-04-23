//! ZIP-upload staging (§4.5).
//!
//! Extracts a plugin archive to a temp directory, parses + validates its
//! `plugin.toml`, and returns a `StagedPlugin` the control plane can then
//! move to `~/.execlaw/plugins/<id>/<version>/`.
//!
//! Install/enable semantics proper are Phase 2; this module exists so
//! Phase 0 can round-trip a manifest through the ZIP format end-to-end.

use crate::manifest::{ManifestError, PluginManifest};
use std::io::{Read, Seek};
use std::path::PathBuf;
use tempfile::TempDir;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StageError {
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("plugin.toml is missing from the archive")]
    MissingManifest,
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("archive contains unsafe path: '{0}' (zip-slip prevented)")]
    UnsafePath(String),
}

/// Result of staging a plugin ZIP to a temp directory.
#[derive(Debug)]
pub struct StagedPlugin {
    /// Parsed manifest.
    pub manifest: PluginManifest,
    /// Temp directory containing the extracted plugin files. Drop to clean up.
    pub tempdir: TempDir,
}

impl StagedPlugin {
    pub fn root(&self) -> PathBuf {
        self.tempdir.path().to_path_buf()
    }
}

/// Extract a ZIP archive into a temp directory and parse its manifest.
pub fn stage_zip<R: Read + Seek>(reader: R) -> Result<StagedPlugin, StageError> {
    let mut zip = zip::ZipArchive::new(reader)?;
    let tempdir = tempfile::tempdir()?;

    // First, extract all files, preventing zip-slip.
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let relative = match file.enclosed_name() {
            Some(p) => p.to_owned(),
            None => return Err(StageError::UnsafePath(file.name().to_owned())),
        };

        let out_path = tempdir.path().join(&relative);
        // Double check containment after join.
        if !out_path.starts_with(tempdir.path()) {
            return Err(StageError::UnsafePath(file.name().to_owned()));
        }

        if file.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut file, &mut out)?;
        }
    }

    // Parse manifest.
    let manifest_path = tempdir.path().join("plugin.toml");
    if !manifest_path.exists() {
        return Err(StageError::MissingManifest);
    }
    let s = std::fs::read_to_string(&manifest_path)?;
    let manifest = PluginManifest::parse(&s)?;

    Ok(StagedPlugin { manifest, tempdir })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn build_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            for (name, contents) in files {
                zw.start_file::<_, ()>(*name, opts).unwrap();
                std::io::Write::write_all(&mut zw, contents.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn stage_a_minimal_plugin_zip() {
        let manifest = r#"
            [plugin]
            id = "hello"
            name = "Hello"
            version = "0.1.0"
        "#;
        let bytes = build_zip(&[("plugin.toml", manifest), ("dist/readme.txt", "ok")]);
        let staged = stage_zip(Cursor::new(bytes)).unwrap();
        assert_eq!(staged.manifest.plugin.id, "hello");
        assert!(staged.root().join("plugin.toml").exists());
        assert!(staged.root().join("dist/readme.txt").exists());
    }

    #[test]
    fn missing_manifest_errors() {
        let bytes = build_zip(&[("dist/readme.txt", "hi")]);
        let err = stage_zip(Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, StageError::MissingManifest));
    }

    #[test]
    fn zipslip_attempt_is_rejected() {
        // Library refuses path components like `..` outright.
        let bytes = build_zip(&[("../evil", "no"), ("plugin.toml", "[plugin]\nid=\"x\"\nname=\"x\"\nversion=\"1\"\n")]);
        let err = stage_zip(Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, StageError::UnsafePath(_)));
    }
}
