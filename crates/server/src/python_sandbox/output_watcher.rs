//! Watch `<work_root>/<convo>/outputs/` for files the kernel writes,
//! debounce while the kernel is still flushing, then fire a callback
//! per finished file.
//!
//! Lives between the kernel sidecar (which writes files into its
//! `/work/<convo>/outputs/` directory) and Phase 5's path-based
//! publish (which turns each fired event into a conversation
//! artifact). Phase 4 only delivers the (`conversation_id`, `path`,
//! `size`) tuple — what to do with it is the callback's problem.
//!
//! Architecture:
//!   * One `RecommendedWatcher` watches the entire `work_root`
//!     recursively. notify spawns an OS thread to deliver events.
//!   * The watcher's closure does the bare minimum: validate the
//!     path layout, then bump a debounce deadline in a shared map.
//!     Heavy work (filesystem stats, user callback) happens on the
//!     tokio timer task.
//!   * A tokio task wakes every `debounce/3` and walks the pending
//!     map. Entries whose deadline has passed fire the user's
//!     callback and are removed.
//!
//! Path discipline:
//!   `<work_root>/<convo>/outputs/<filename>`  → fire
//!   `<work_root>/<convo>/outputs/sub/<file>`  → fire (any depth under outputs/)
//!   `<work_root>/<convo>/uploads/<file>`      → ignore (these are hydration's domain)
//!   `<work_root>/<convo>/<file>`              → ignore (scratch space)
//!   `<work_root>/<file>`                      → ignore (not in any convo)
//!
//! Why not `notify_debouncer_full`: we want explicit control over
//! the debounce policy (300 ms default, configurable; reset on
//! Modify, drop on Remove) so a kernel that streams a 1 GB write
//! over 30 s only fires one callback at the end. notify_debouncer's
//! defaults emit on the first event of a burst.
//!
//! Lifetime: dropping the [`OutputWatcher`] aborts the timer task
//! and drops the underlying `notify::Watcher`, which signals its
//! OS thread to exit. Final pending events that haven't crossed
//! the deadline are silently dropped — same semantics as a process
//! restart, which the system handles via state reconciliation
//! (Phase 8 startup pass).

use execlaw_core::ids::ConversationId;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

/// Per-path debounce state. We track size at the last-observed
/// event so we can detect "writes are still arriving but notify
/// hasn't delivered the events yet" — a real failure mode on
/// Windows where `ReadDirectoryChangesW` buffers events into
/// batches separated by gaps long enough to trip our debounce
/// deadline. Audit caught this: a 32-chunk write at 5 ms intervals
/// fired the callback twice without this guard.
#[derive(Debug, Clone, Copy)]
struct PendingEntry {
    last_event: Instant,
    size_at_last_event: u64,
}

/// Default debounce window. A 300 ms-no-writes silence is taken as
/// "the kernel finished writing this file." Long enough to coalesce
/// the chunked writes Python's `to_csv` / `to_parquet` produce;
/// short enough that the user sees the artifact chip within ~1s of
/// the cell ending.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// What the watcher hands to the callback when a file is ready.
#[derive(Debug, Clone)]
pub struct OutputCreated {
    pub conversation_id: ConversationId,
    /// Absolute host-side path. Container sees the same byte content
    /// at the bind-mount equivalent (`/work/<convo>/outputs/...`).
    pub path: PathBuf,
    /// Size in bytes at debounce-fire time. Captured here so the
    /// callback doesn't have to stat the file (and race with a
    /// late write that slipped past the deadline).
    pub size: u64,
}

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("filesystem watcher setup failed: {0}")]
    Notify(#[from] notify::Error),
}

/// Cheap handle. Dropping it stops the watcher.
pub struct OutputWatcher {
    timer_handle: tokio::task::JoinHandle<()>,
    // Hold the watcher so its drop signals the OS thread on shutdown.
    _watcher: RecommendedWatcher,
}

impl Drop for OutputWatcher {
    fn drop(&mut self) {
        self.timer_handle.abort();
    }
}

impl OutputWatcher {
    /// Start watching `work_root` recursively. The closure fires
    /// for each file under `<work_root>/<convo>/outputs/` once it
    /// has been quiescent for `debounce`. Callback MUST be cheap;
    /// for async work spawn a tokio task inside it.
    pub fn start<F>(
        work_root: PathBuf,
        debounce: Duration,
        callback: F,
    ) -> Result<Self, WatchError>
    where
        F: Fn(OutputCreated) + Send + Sync + 'static,
    {
        // Ensure work_root exists so notify::watch doesn't fail on
        // first call. Idempotent.
        std::fs::create_dir_all(&work_root).map_err(|e| {
            WatchError::Notify(notify::Error::generic(&format!(
                "create work_root {}: {e}",
                work_root.display()
            )))
        })?;
        let work_root = work_root
            .canonicalize()
            .unwrap_or(work_root.clone());

        let pending: Arc<Mutex<HashMap<PathBuf, PendingEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_for_watch = pending.clone();
        let root_for_watch = work_root.clone();

        // Notify's event closure. Runs on notify's OS thread.
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let event = match res {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(?e, "notify watcher reported error; continuing");
                    return;
                }
            };
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    let now = Instant::now();
                    for path in event.paths.iter() {
                        if !is_in_outputs_dir(&root_for_watch, path) {
                            continue;
                        }
                        // Sample the size at event time so the
                        // timer's stability check has a baseline
                        // it can compare against. `unwrap_or(0)`
                        // covers the "file deleted between
                        // notify firing and us stat'ing" race —
                        // the stability check on the timer side
                        // catches this on the next pass.
                        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        pending_for_watch.lock().unwrap().insert(
                            path.clone(),
                            PendingEntry {
                                last_event: now,
                                size_at_last_event: size,
                            },
                        );
                    }
                }
                EventKind::Remove(_) => {
                    for path in event.paths.iter() {
                        if !is_in_outputs_dir(&root_for_watch, path) {
                            continue;
                        }
                        // Drop any pending deadline — the file is
                        // gone, no callback owed.
                        pending_for_watch.lock().unwrap().remove(path);
                    }
                }
                // Access / Other events don't change pending state.
                _ => {}
            }
        })?;

        watcher.watch(&work_root, RecursiveMode::Recursive)?;

        // Tokio timer task — wakes every debounce/3 and walks the
        // pending map. Fires only when BOTH the debounce window
        // has elapsed since the last event AND the file size has
        // stopped growing (defends against Windows
        // ReadDirectoryChangesW buffering writes into batches
        // separated by gaps longer than our debounce).
        let pending_for_timer = pending.clone();
        let work_root_for_timer = work_root.clone();
        let tick = debounce / 3;
        let callback = Arc::new(callback);
        let timer_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tick);
            loop {
                ticker.tick().await;
                let now = Instant::now();
                // Two-phase: collect (path, fire_now, new_entry_if_extending)
                // tuples under the lock, then fire callbacks outside
                // the lock so a slow user closure doesn't block the
                // watcher's writer.
                let mut to_fire: Vec<(PathBuf, ConversationId, u64)> = Vec::new();
                {
                    let mut map = pending_for_timer.lock().unwrap();
                    let candidates: Vec<PathBuf> = map
                        .iter()
                        .filter(|(_, e)| now.duration_since(e.last_event) >= debounce)
                        .map(|(p, _)| p.clone())
                        .collect();
                    for path in candidates {
                        // Stability check: stat the file NOW under
                        // the lock. If size matches what we had at
                        // last_event, no writes happened in the
                        // intervening debounce window → safe to fire.
                        // If size differs, there was a buffered
                        // write notify hasn't delivered yet —
                        // extend the deadline and wait another cycle.
                        let cur_size = match std::fs::metadata(&path) {
                            Ok(m) if m.is_file() => m.len(),
                            // File missing or non-file — drop the entry.
                            _ => {
                                map.remove(&path);
                                continue;
                            }
                        };
                        let entry = match map.get(&path) {
                            Some(e) => *e,
                            None => continue, // Concurrent removal.
                        };
                        if cur_size != entry.size_at_last_event {
                            // File still growing under us. Update
                            // the baseline and wait at least another
                            // debounce window. This is the case the
                            // Phase 4 audit caught.
                            map.insert(
                                path,
                                PendingEntry {
                                    last_event: now,
                                    size_at_last_event: cur_size,
                                },
                            );
                            continue;
                        }
                        // Size stable for the full debounce window.
                        // Fire and remove.
                        if let Some((convo, size)) = resolve_output(&work_root_for_timer, &path)
                        {
                            to_fire.push((path.clone(), convo, size));
                        }
                        map.remove(&path);
                    }
                }
                for (path, convo, size) in to_fire {
                    callback(OutputCreated {
                        conversation_id: convo,
                        path,
                        size,
                    });
                }
            }
        });

        tracing::info!(
            work_root = %work_root.display(),
            debounce_ms = debounce.as_millis(),
            "python_sandbox output watcher started"
        );

        Ok(OutputWatcher {
            timer_handle,
            _watcher: watcher,
        })
    }
}

/// Path layout check: returns true iff `path` is somewhere under
/// `<work_root>/<convo>/outputs/`. Tolerates paths that don't
/// exist yet (notify reports Create events before the file is
/// fully on disk).
fn is_in_outputs_dir(work_root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(work_root) else {
        return false;
    };
    let mut comps = rel.components();
    let _convo = match comps.next() {
        Some(std::path::Component::Normal(_)) => (),
        _ => return false,
    };
    let outputs = match comps.next() {
        Some(std::path::Component::Normal(s)) => s,
        _ => return false,
    };
    if outputs != std::ffi::OsStr::new("outputs") {
        return false;
    }
    // At least one path segment after `outputs/` (the filename).
    comps.next().is_some()
}

/// Extract `(ConversationId, file_size)` for an output path that
/// passed [`is_in_outputs_dir`]. Returns `None` if the file is
/// gone by the time we look — common when a tool deletes its own
/// scratch outputs before the debounce fires.
fn resolve_output(work_root: &Path, path: &Path) -> Option<(ConversationId, u64)> {
    let rel = path.strip_prefix(work_root).ok()?;
    let convo_segment = rel.components().next()?;
    let convo_name = match convo_segment {
        std::path::Component::Normal(s) => s.to_string_lossy().into_owned(),
        _ => return None,
    };
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        // Directory or symlink, skip.
        return None;
    }
    Some((ConversationId::from(convo_name), meta.len()))
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn convo(s: &str) -> ConversationId {
        ConversationId::from(s.to_string())
    }

    #[test]
    fn is_in_outputs_dir_accepts_real_layout() {
        let root = PathBuf::from("/work");
        assert!(is_in_outputs_dir(&root, Path::new("/work/c-1/outputs/a.csv")));
        assert!(is_in_outputs_dir(
            &root,
            Path::new("/work/c-1/outputs/sub/a.png")
        ));
    }

    #[test]
    fn is_in_outputs_dir_rejects_other_layouts() {
        let root = PathBuf::from("/work");
        // uploads is not ours
        assert!(!is_in_outputs_dir(&root, Path::new("/work/c-1/uploads/a.csv")));
        // scratch at convo root
        assert!(!is_in_outputs_dir(&root, Path::new("/work/c-1/scratch.txt")));
        // outside any convo
        assert!(!is_in_outputs_dir(&root, Path::new("/work/loose.txt")));
        // outside work_root entirely
        assert!(!is_in_outputs_dir(&root, Path::new("/etc/passwd")));
        // exactly the outputs dir, no filename
        assert!(!is_in_outputs_dir(&root, Path::new("/work/c-1/outputs")));
    }

    /// Wait for `predicate` to become true within `timeout`, polling
    /// every 20 ms via tokio::time::sleep so the tokio executor
    /// keeps scheduling the watcher's timer task. Using
    /// `std::thread::sleep` here would starve the timer and the
    /// callback would never fire — verified the hard way the first
    /// time this test was written.
    async fn await_until_async<F: FnMut() -> bool>(
        mut predicate: F,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        predicate()
    }

    #[tokio::test]
    async fn fires_callback_for_outputs_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let _w = OutputWatcher::start(root.clone(), Duration::from_millis(150), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();

        // Give notify a beat to register the recursive watch.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let path = root.join("c-1").join("outputs");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("hello.txt"), b"hi").unwrap();

        let got = await_until_async(|| rx.try_recv().is_ok(), Duration::from_secs(2)).await;
        // try_recv "consumed" any event already; redo to capture for assertions.
        // Either way the predicate confirms an event arrived.
        assert!(got, "callback should have fired for an outputs/ write");
    }

    #[tokio::test]
    async fn ignores_writes_outside_outputs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let fired = Arc::new(Mutex::new(false));
        let fired_for_cb = fired.clone();
        let _w = OutputWatcher::start(root.clone(), Duration::from_millis(100), move |_| {
            *fired_for_cb.lock().unwrap() = true;
        })
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        // Write under /uploads (forbidden) and at convo root (forbidden).
        std::fs::create_dir_all(root.join("c-1").join("uploads")).unwrap();
        std::fs::write(root.join("c-1").join("uploads").join("u.csv"), b"x").unwrap();
        std::fs::write(root.join("c-1").join("scratch.txt"), b"x").unwrap();
        std::fs::write(root.join("loose.txt"), b"x").unwrap();

        // Wait at least 3x debounce so any spurious callback would have fired.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !*fired.lock().unwrap(),
            "non-outputs writes must NOT fire the callback"
        );
    }

    #[tokio::test]
    async fn debounces_rapid_writes_to_one_callback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let _w = OutputWatcher::start(root.clone(), Duration::from_millis(200), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let outputs = root.join("c-1").join("outputs");
        std::fs::create_dir_all(&outputs).unwrap();
        let p = outputs.join("growing.csv");

        // Simulate the kernel writing in chunks faster than the
        // debounce window. Each write resets the deadline.
        for i in 0..5 {
            std::fs::write(&p, vec![b'x'; (i + 1) * 1000]).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Now go silent. After debounce, exactly ONE callback should fire.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut events = 0;
        while rx.try_recv().is_ok() {
            events += 1;
        }
        assert_eq!(events, 1, "rapid writes must coalesce to one callback");
    }

    #[tokio::test]
    async fn callback_carries_correct_convo_id_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let _w = OutputWatcher::start(root.clone(), Duration::from_millis(120), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let outputs = root.join("convo-XYZ").join("outputs");
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(outputs.join("region.csv"), b"abcdef").unwrap();

        let event = {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut got = None;
            while Instant::now() < deadline {
                if let Ok(e) = rx.try_recv() {
                    got = Some(e);
                    break;
                }
                // tokio::time::sleep yields so the timer task runs.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            got.expect("event should fire within 2s")
        };
        assert_eq!(event.conversation_id, convo("convo-XYZ"));
        assert_eq!(event.size, 6);
        assert!(event.path.ends_with("region.csv"));
    }

    #[tokio::test]
    async fn deleted_before_debounce_does_not_fire() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let fired = Arc::new(Mutex::new(0u32));
        let fired_for_cb = fired.clone();
        let _w = OutputWatcher::start(root.clone(), Duration::from_millis(250), move |_| {
            *fired_for_cb.lock().unwrap() += 1;
        })
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let outputs = root.join("c-1").join("outputs");
        std::fs::create_dir_all(&outputs).unwrap();
        let p = outputs.join("ephemeral.csv");
        std::fs::write(&p, b"x").unwrap();
        // Remove before debounce window expires.
        tokio::time::sleep(Duration::from_millis(80)).await;
        std::fs::remove_file(&p).unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            *fired.lock().unwrap(),
            0,
            "file deleted before debounce must NOT fire callback"
        );
    }

    #[tokio::test]
    async fn distinct_files_fire_distinct_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let _w = OutputWatcher::start(root.clone(), Duration::from_millis(150), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let a = root.join("c-1").join("outputs");
        let b = root.join("c-2").join("outputs");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("alpha.csv"), b"a").unwrap();
        std::fs::write(b.join("beta.csv"), b"b").unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut convos: Vec<String> = Vec::new();
        while let Ok(e) = rx.try_recv() {
            convos.push(e.conversation_id.as_str().to_string());
        }
        convos.sort();
        assert_eq!(
            convos,
            vec!["c-1".to_string(), "c-2".to_string()],
            "each convo should get its own callback"
        );
    }
}
