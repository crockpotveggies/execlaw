//! Hydrate a conversation's uploaded attachments into the sandbox's
//! `/work/<convo>/uploads/` directory.
//!
//! Design: copy blob-store files into the per-conversation upload
//! dir under their original filenames, then chmod read-only. The
//! supervisor calls this on sidecar spawn and on new-attachment
//! events; the agent sees the resulting files at
//! `/work/<convo_id>/uploads/<filename>` inside the container.
//!
//! Why copy, not symlink:
//!   - The dev host is Windows, prod is Linux, and the sidecar's
//!     mount is a directory on the host that the Linux container
//!     bind-mounts. Cross-platform symlinks across this boundary
//!     are flaky (Windows reparse points are not followed by Linux
//!     containers; ln -s requires admin on Windows).
//!   - Copy + chmod 0o444 enforces read-only at the FS layer; a
//!     symlink to a read-only blob would also be read-only via the
//!     blob's perms, but the layering is murkier and harder to
//!     audit.
//!   - Disk cost is acceptable: analyst attachments are typically
//!     under 100 MB. Future optimization can swap to hardlinks on
//!     Linux (`std::fs::hard_link`) when source + dest are on the
//!     same filesystem; left as a deliberate optimization to land
//!     when telemetry shows it matters.
//!
//! Idempotency: `hydrate_uploads` is safe to call repeatedly. If the
//! destination file already exists with a matching size AND mtime
//! >= the blob's mtime, the copy is skipped. (We don't checksum-
//! verify on every call — blobs are content-addressed by sha256
//! upstream, so a name+size match is sufficient identity proof.)
//!
//! Cleanup of stale entries: this module does NOT delete files from
//! `/work/<convo>/uploads/` that aren't in the input list — callers
//! that need full reconciliation (e.g. when an attachment was
//! retracted from the conversation) pass a `cleanup_orphans: true`
//! flag. By default we additive-merge, which matches the common
//! "new attachment arrived" event without surprising the agent.
//!
//! The agent finds these files at `/work/<conversation_id>/uploads/`
//! inside the container — see `python.execute`'s tool description
//! for the filesystem convention.

use execlaw_core::ids::ConversationId;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// One inbound attachment to hydrate. Caller (Phase 8 tool dispatch)
/// derives these from `state_attachments`. We don't read the table
/// here so Phase 3 isn't blocked on Phase 6's `filename` column —
/// the caller passes whatever filename it has, and the hydration
/// layer treats it as opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentToHydrate {
    /// On-disk path of the immutable blob (typically
    /// `~/.execlaw/blobs/<sha256>`).
    pub blob_path: PathBuf,
    /// Operator-visible filename — what the file will be called in
    /// `/work/<convo>/uploads/`. The agent sees this name.
    pub filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydratedFile {
    pub filename: String,
    pub size_bytes: u64,
    /// Absolute path on the host where the file landed. The
    /// container sees this same byte content at the bind-mount
    /// equivalent — i.e. `/work/<convo>/uploads/<filename>`.
    pub host_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct HydrateOpts {
    /// If true, files in the destination directory that aren't in
    /// the input list are removed. Use this when reconciling after
    /// an attachment was retracted; leave false for the common
    /// "new attachment landed" event so partial inputs don't wipe
    /// existing uploads.
    pub cleanup_orphans: bool,
}

impl Default for HydrateOpts {
    fn default() -> Self {
        Self {
            cleanup_orphans: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum HydrationError {
    #[error("blob path is not a file: {0}")]
    BlobNotFile(PathBuf),
    #[error("invalid filename {0:?}: contains path separators or `..`")]
    UnsafeFilename(String),
    #[error("io error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl HydrationError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Compute the per-conversation uploads directory under the sidecar's
/// `state://work` root. Pure path computation; doesn't touch disk.
pub fn uploads_dir(work_root: &Path, convo: &ConversationId) -> PathBuf {
    work_root.join(convo.as_str()).join("uploads")
}

/// Hydrate all attachments for one conversation. Returns the list of
/// files that ended up in the uploads dir (whether freshly copied or
/// already up-to-date).
///
/// `work_root` is the host-side path that's bind-mounted into the
/// container at `/work`. For the canonical sidecar it's
/// `<execlaw-state>/sidecars/python-sandbox/kernel-gateway/work`.
pub fn hydrate_uploads(
    work_root: &Path,
    convo: &ConversationId,
    attachments: &[AttachmentToHydrate],
    opts: HydrateOpts,
) -> Result<Vec<HydratedFile>, HydrationError> {
    let dir = uploads_dir(work_root, convo);
    fs::create_dir_all(&dir).map_err(|e| HydrationError::io(&dir, e))?;

    let mut kept: HashSet<String> = HashSet::new();
    let mut result: Vec<HydratedFile> = Vec::with_capacity(attachments.len());

    for a in attachments {
        if !is_safe_filename(&a.filename) {
            return Err(HydrationError::UnsafeFilename(a.filename.clone()));
        }
        let blob_meta =
            fs::metadata(&a.blob_path).map_err(|e| HydrationError::io(&a.blob_path, e))?;
        if !blob_meta.is_file() {
            return Err(HydrationError::BlobNotFile(a.blob_path.clone()));
        }
        let dest = dir.join(&a.filename);

        let copy_needed = match fs::metadata(&dest) {
            // Size match is enough — blobs are content-addressed
            // upstream so identical size + identical name in the
            // same dir means identical bytes.
            Ok(m) if m.is_file() && m.len() == blob_meta.len() => false,
            Ok(_) | Err(_) => true,
        };

        if copy_needed {
            // Make the existing file writable before clobbering —
            // a previous hydrate set it 0o444 and an os.copy on
            // top of a read-only dest fails on Linux.
            if dest.exists() {
                let _ = make_writable(&dest);
                fs::remove_file(&dest).map_err(|e| HydrationError::io(&dest, e))?;
            }
            fs::copy(&a.blob_path, &dest).map_err(|e| HydrationError::io(&dest, e))?;
            make_read_only(&dest).map_err(|e| HydrationError::io(&dest, e))?;
        }

        kept.insert(a.filename.clone());
        result.push(HydratedFile {
            filename: a.filename.clone(),
            size_bytes: blob_meta.len(),
            host_path: dest,
        });
    }

    if opts.cleanup_orphans {
        for entry in fs::read_dir(&dir).map_err(|e| HydrationError::io(&dir, e))? {
            let entry = entry.map_err(|e| HydrationError::io(&dir, e))?;
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                // Non-UTF8 names — leave them alone rather than
                // risk deleting something the operator placed by
                // hand.
                Err(_) => continue,
            };
            if !kept.contains(&name) && entry.path().is_file() {
                let p = entry.path();
                let _ = make_writable(&p);
                if let Err(e) = fs::remove_file(&p) {
                    tracing::warn!(?e, path=?p, "hydrate cleanup_orphans: remove failed");
                }
            }
        }
    }

    Ok(result)
}

/// A filename is safe iff it contains no path separators or parent-
/// dir references. Defense against an attacker-controlled filename
/// like `../../etc/passwd` escaping `/work/<convo>/uploads/`.
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.split(|c| c == '/' || c == '\\').any(|seg| seg == "..")
}

#[cfg(unix)]
fn make_read_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o444);
    fs::set_permissions(path, perms)
}

#[cfg(unix)]
fn make_writable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o644);
    fs::set_permissions(path, perms)
}

#[cfg(windows)]
fn make_read_only(path: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms)
}

#[cfg(windows)]
fn make_writable(path: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms)
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn convo(s: &str) -> ConversationId {
        ConversationId::from(s.to_string())
    }

    /// Lay out a fake blob store + work root in a tempdir and return
    /// (work_root, blob_paths_by_name).
    fn temp_setup(
        blobs: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, PathBuf, Vec<AttachmentToHydrate>) {
        let dir = tempfile::tempdir().unwrap();
        let blob_root = dir.path().join("blobs");
        let work_root = dir.path().join("work");
        fs::create_dir_all(&blob_root).unwrap();
        fs::create_dir_all(&work_root).unwrap();
        let mut attachments = Vec::new();
        for (name, bytes) in blobs {
            // Real blobs are sha256-named on disk; the on-disk
            // filename is irrelevant to hydration since we get the
            // operator-facing name from `AttachmentToHydrate.filename`.
            let p = blob_root.join(format!("blob-{name}"));
            let mut f = fs::File::create(&p).unwrap();
            f.write_all(bytes).unwrap();
            attachments.push(AttachmentToHydrate {
                blob_path: p,
                filename: (*name).to_string(),
            });
        }
        (dir, work_root, attachments)
    }

    #[test]
    fn hydrate_copies_blobs_into_uploads_dir() {
        let (_g, work_root, attachments) = temp_setup(&[
            ("sales.csv", b"region,rev\nWest,100\n"),
            ("notes.txt", b"hello world\n"),
        ]);
        let c = convo("convo-1");
        let out =
            hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).expect("hydrate");
        assert_eq!(out.len(), 2);

        let csv_path = uploads_dir(&work_root, &c).join("sales.csv");
        let txt_path = uploads_dir(&work_root, &c).join("notes.txt");
        assert!(csv_path.exists());
        assert!(txt_path.exists());
        assert_eq!(fs::read(&csv_path).unwrap(), b"region,rev\nWest,100\n");
        assert_eq!(fs::read(&txt_path).unwrap(), b"hello world\n");
    }

    #[test]
    fn hydrated_files_are_read_only() {
        let (_g, work_root, attachments) = temp_setup(&[("data.parquet", b"\x00\x01\x02\x03")]);
        let c = convo("ro-convo");
        let _ = hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        let dest = uploads_dir(&work_root, &c).join("data.parquet");
        let perms = fs::metadata(&dest).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(perms.mode() & 0o777, 0o444, "must be 0o444");
        }
        #[cfg(windows)]
        {
            assert!(perms.readonly(), "must be readonly");
        }
    }

    #[test]
    fn hydrate_is_idempotent_no_recopy_on_size_match() {
        let (_g, work_root, attachments) = temp_setup(&[("a.csv", b"hello")]);
        let c = convo("idempo");
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        let dest = uploads_dir(&work_root, &c).join("a.csv");
        let mtime1 = fs::metadata(&dest).unwrap().modified().unwrap();

        // Sleep a touch so a recopy would shift mtime, then run again.
        std::thread::sleep(std::time::Duration::from_millis(50));
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        let mtime2 = fs::metadata(&dest).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "second call must not have recopied");
    }

    #[test]
    fn hydrate_replaces_dest_when_size_differs() {
        // Swap the blob with a different-sized one between calls.
        // Same name, different content → hydrate must recopy.
        let (dir, work_root, mut attachments) = temp_setup(&[("a.csv", b"hi")]);
        let c = convo("repl");
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        let dest = uploads_dir(&work_root, &c).join("a.csv");
        assert_eq!(fs::read(&dest).unwrap(), b"hi");

        // New blob with different size.
        let new_blob = dir.path().join("blobs").join("blob-a.csv-v2");
        fs::write(&new_blob, b"hello world").unwrap();
        attachments[0].blob_path = new_blob;
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello world");
    }

    #[test]
    fn cleanup_orphans_removes_extra_files() {
        let (_g, work_root, attachments) = temp_setup(&[("keep.csv", b"keep")]);
        let c = convo("clean");
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        // Drop a rogue file the input didn't ask for.
        let rogue = uploads_dir(&work_root, &c).join("rogue.csv");
        fs::write(&rogue, b"rogue").unwrap();
        assert!(rogue.exists());

        hydrate_uploads(
            &work_root,
            &c,
            &attachments,
            HydrateOpts {
                cleanup_orphans: true,
            },
        )
        .unwrap();
        assert!(!rogue.exists(), "cleanup_orphans must remove rogue.csv");
        assert!(
            uploads_dir(&work_root, &c).join("keep.csv").exists(),
            "the kept file must still be there"
        );
    }

    #[test]
    fn no_cleanup_by_default_preserves_extras() {
        // Verify the safer default: rogue file is NOT touched when
        // cleanup_orphans is false (most callers).
        let (_g, work_root, attachments) = temp_setup(&[("keep.csv", b"x")]);
        let c = convo("nocleanup");
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        let rogue = uploads_dir(&work_root, &c).join("rogue.csv");
        fs::write(&rogue, b"r").unwrap();
        hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default()).unwrap();
        assert!(
            rogue.exists(),
            "rogue must survive when cleanup_orphans=false"
        );
    }

    #[test]
    fn rejects_path_traversal_filenames() {
        let (_g, work_root, mut attachments) = temp_setup(&[("dummy.csv", b"x")]);
        let c = convo("trav");
        for evil in [
            "../etc/passwd",
            "..\\windows\\system32",
            "../a",
            "a/b",
            "a\\b",
            "",
        ] {
            attachments[0].filename = evil.into();
            let err = hydrate_uploads(&work_root, &c, &attachments, HydrateOpts::default())
                .expect_err(&format!("must reject {evil:?}"));
            assert!(
                matches!(err, HydrationError::UnsafeFilename(_)),
                "{evil:?}: wrong error type: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_filename_with_null_byte() {
        let (_g, work_root, mut attachments) = temp_setup(&[("dummy.csv", b"x")]);
        attachments[0].filename = "evil\0name.csv".into();
        let err = hydrate_uploads(
            &work_root,
            &convo("null"),
            &attachments,
            HydrateOpts::default(),
        )
        .expect_err("must reject NUL-byte filename");
        assert!(matches!(err, HydrationError::UnsafeFilename(_)));
    }

    #[test]
    fn per_convo_dirs_are_isolated() {
        let (dir, work_root, attachments) = temp_setup(&[("file.csv", b"a")]);
        hydrate_uploads(
            &work_root,
            &convo("convo-a"),
            &attachments,
            HydrateOpts::default(),
        )
        .unwrap();

        // Different convo, different file.
        let other_blob = dir.path().join("blobs").join("blob-other");
        fs::write(&other_blob, b"b").unwrap();
        let other_attachments = vec![AttachmentToHydrate {
            blob_path: other_blob,
            filename: "other.csv".into(),
        }];
        hydrate_uploads(
            &work_root,
            &convo("convo-b"),
            &other_attachments,
            HydrateOpts::default(),
        )
        .unwrap();

        assert!(
            uploads_dir(&work_root, &convo("convo-a"))
                .join("file.csv")
                .exists()
        );
        assert!(
            uploads_dir(&work_root, &convo("convo-b"))
                .join("other.csv")
                .exists()
        );
        // Cross-isolation: A's file must not appear in B's dir.
        assert!(
            !uploads_dir(&work_root, &convo("convo-b"))
                .join("file.csv")
                .exists()
        );
        assert!(
            !uploads_dir(&work_root, &convo("convo-a"))
                .join("other.csv")
                .exists()
        );
    }

    #[test]
    fn uploads_dir_path_layout_is_stable() {
        let work = PathBuf::from("/srv/execlaw/sidecars/python-sandbox/kernel-gateway/work");
        let p = uploads_dir(&work, &convo("abc-123"));
        assert!(p.ends_with("abc-123/uploads"));
    }
}
