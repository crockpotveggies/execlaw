//! Thin Rust wrapper over `systemctl --user`.
//!
//! Linux analogue of `desktop-macos/src-tauri/src/sm_app_service.rs`
//! (macOS `SMAppService`) and `desktop-windows/src-tauri/src/scm.rs`
//! (Windows SCM). Whereas macOS uses `SMAppService` and Windows uses
//! the Service Control Manager, Linux uses `systemd --user` units
//! living under `~/.config/systemd/user/`.
//!
//! Install model: the bundled `execlaw` CLI (see
//! `crates/cli/src/service.rs`) writes the unit file via the
//! `service-manager` crate's systemd backend and starts it via
//! `systemctl --user start`. The tray app on first launch calls
//! `execlaw service install --user` + `service start --user` — the
//! same idempotent pattern the macOS tray uses with
//! `SMAppService.register()`. User-level units don't need any
//! privilege escalation (they live in the operator's HOME and
//! systemd-user runs them under the operator's UID), so there's no
//! `pkexec` / `sudo` plumbing here — the contrast with the Windows
//! tray's `ShellExecuteW("runas")` UAC dance.
//!
//! ## Why shell out to `systemctl` instead of libsystemd FFI
//!
//! The `systemd-rs` family of crates requires `libsystemd-dev` at
//! build time. Shipping a build dep just to read three property
//! values (`ActiveState`, `UnitFileState`, `LoadState`) doesn't pay
//! its weight — the `systemctl` invocations parse cleanly, fit on
//! one screen each, and don't drag in another transitive dep tree.

#![cfg(target_os = "linux")]

use std::process::Stdio;
use tokio::process::Command;

/// Stable identifier for the systemd unit. Must match
/// `crates/cli/src/service.rs::SERVICE_LABEL`. If that constant ever
/// changes, this must change in lockstep. The `.service` suffix is
/// what systemctl expects on the command line — appended at every
/// call site rather than baked into the constant so a future
/// `.timer` companion unit can share the same label.
pub const SERVICE_LABEL: &str = "execlaw";

/// Tray-side view of the systemd unit state. Mirrors the macOS
/// `AgentStatus` and Windows `SvcStatus` enums so the
/// `compose_status_label` logic in `app.rs` can match the other
/// platforms arm-for-arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitStatus {
    /// `systemctl --user list-unit-files execlaw.service` returned
    /// no rows. The tray's first-launch register flow puts the file
    /// in place; this state means either it hasn't run yet or the
    /// operator removed it manually.
    NotInstalled,
    /// `ActiveState = active` — service running normally.
    Running,
    /// `ActiveState = inactive` (and the unit IS installed). Either
    /// it's been stopped or it crashed without RestartPolicy.
    Stopped,
    /// `ActiveState = activating` / `deactivating` / `reloading`.
    /// Transient — collapsed into one bucket because the tray
    /// status row can't usefully distinguish them at 5-second poll
    /// intervals.
    Pending,
    /// `ActiveState = failed`. Differentiated from `Stopped` so the
    /// tray can surface a "view logs" affordance.
    Failed,
    /// Couldn't run `systemctl --user` at all (binary missing, no
    /// systemd session bus, etc.). Surfaced as a soft error rather
    /// than a panic — the tray still functions for the chat-window
    /// affordance even when the service can't be supervised.
    Error,
}

impl UnitStatus {
    /// Parse the single-word output of
    /// `systemctl --user is-active execlaw.service`. systemd
    /// guarantees one of a fixed set of strings on stdout.
    fn from_is_active(s: &str) -> Self {
        match s.trim() {
            "active" => Self::Running,
            "inactive" => Self::Stopped,
            "failed" => Self::Failed,
            "activating" | "deactivating" | "reloading" => Self::Pending,
            // `unknown` from `is-active` means the unit isn't loaded
            // at all — covers both "not installed" and "installed
            // but failed to parse." We map to NotInstalled and let
            // a follow-up `list-unit-files` call disambiguate when
            // the tray needs to (it currently doesn't).
            "unknown" => Self::NotInstalled,
            _ => Self::Error,
        }
    }
}

/// Errors bubbling up from `systemctl`. The user-facing strings are
/// kept short — `systemctl`'s own stderr is the source of truth and
/// the tray surfaces it verbatim when displaying these.
#[derive(Debug, thiserror::Error)]
pub enum SystemdError {
    #[error("systemctl binary not found on PATH — is systemd installed?")]
    Missing,
    #[error("systemctl exited {code}: {stderr}")]
    Nonzero { code: i32, stderr: String },
    #[error("could not spawn systemctl: {0}")]
    Spawn(String),
}

/// Single-shot query: returns the current service state. Used by the
/// tray's 5-second poller. Never errors — failures (no systemctl,
/// no user session) collapse to `UnitStatus::Error` so the tray
/// stays responsive.
pub async fn query() -> UnitStatus {
    let unit = format!("{SERVICE_LABEL}.service");
    let result = Command::new("systemctl")
        .args(["--user", "is-active", "--", unit.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    let output = match result {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "systemctl --user is-active spawn failed");
            return UnitStatus::Error;
        }
    };
    // `is-active` exits nonzero when the unit is inactive / failed
    // — that's normal output, not an error from our perspective.
    // We read stdout regardless of the exit code.
    let stdout = String::from_utf8_lossy(&output.stdout);
    UnitStatus::from_is_active(&stdout)
}

/// Register the systemd user unit by shelling out to the bundled
/// `execlaw` CLI. The CLI's `service install --user` writes
/// `~/.config/systemd/user/execlaw.service` via the
/// `service-manager` crate's systemd backend (same code path
/// covered by the CLI's own integration tests). Idempotent — a
/// second register is a no-op + a status refresh.
///
/// Mirrors the macOS tray's call to `SMAppService.register()` at
/// app launch and the Windows installer's NSIS post-install hook
/// that runs `execlaw.exe service install --system`.
pub async fn register_user_service(tray_exe_dir: &std::path::Path) -> Result<(), SystemdError> {
    run_execlaw_service_verb(tray_exe_dir, "install").await
}

/// Trigger `execlaw service start --user`. Used by the first-launch
/// flow right after `register_user_service`. Also wired to the
/// tray's "Restart service" menu item (via `restart_user_service`,
/// which sequences `stop` + `start`).
pub async fn start_user_service(tray_exe_dir: &std::path::Path) -> Result<(), SystemdError> {
    run_execlaw_service_verb(tray_exe_dir, "start").await
}

/// Restart by stop + start. Mirrors `service-manager`'s lack of a
/// direct restart verb (the CLI's `service restart` does the same
/// sequence internally). Errors on stop are downgraded to a warning
/// because stopping an already-stopped service is fine.
pub async fn restart_user_service(tray_exe_dir: &std::path::Path) -> Result<(), SystemdError> {
    if let Err(e) = run_execlaw_service_verb(tray_exe_dir, "stop").await {
        tracing::warn!(error = %e, "stop step of restart returned error; continuing");
    }
    run_execlaw_service_verb(tray_exe_dir, "start").await
}

/// Trigger `execlaw service uninstall --user`. Removes the unit
/// file + clears the systemd-user registry entry. Used by the
/// tray's "Uninstall execlaw…" menu item for parity with the
/// macOS / Windows uninstall flows.
pub async fn uninstall_user_service(tray_exe_dir: &std::path::Path) -> Result<(), SystemdError> {
    // `service stop` first so the unit isn't running when we tear
    // its file down — matches what the Windows pre-uninstall hook
    // does.
    if let Err(e) = run_execlaw_service_verb(tray_exe_dir, "stop").await {
        tracing::warn!(error = %e, "stop step of uninstall returned error; continuing");
    }
    run_execlaw_service_verb(tray_exe_dir, "uninstall").await
}

/// Run `execlaw <verb> --user` (where verb is one of `service
/// install`, `service start`, `service stop`, `service uninstall`).
/// Captures stderr for error messages so the tray can surface them
/// in a dialog.
async fn run_execlaw_service_verb(
    tray_exe_dir: &std::path::Path,
    verb: &str,
) -> Result<(), SystemdError> {
    let bundled_exe = bundled_execlaw_path(tray_exe_dir);
    if !bundled_exe.exists() {
        return Err(SystemdError::Missing);
    }
    let output = Command::new(&bundled_exe)
        .args(["service", verb, "--"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SystemdError::Spawn(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Err(SystemdError::Nonzero {
        code: output.status.code().unwrap_or(-1),
        stderr,
    })
}

/// Open a journalctl viewer for the unit. Best-effort — uses
/// `xdg-open` against a `journal://`-style URL is NOT a real thing
/// on Linux, so we fall back to spawning `gnome-system-monitor` or
/// a terminal running `journalctl --user -u execlaw -f`. For the
/// tray's "View logs" affordance, we prefer the terminal path
/// because it's the most-universally-available + most useful for
/// the kind of operator who debugs systemd units.
pub async fn open_journal_viewer() {
    // Preference order: gnome-terminal > konsole > xfce4-terminal >
    // xterm. First one on PATH wins. Each gets the same args
    // (`-e journalctl --user -u execlaw -f`); the flag for "run
    // this command inside the terminal" varies enough that we
    // codify it per-terminal.
    let cmds: &[(&str, &[&str])] = &[
        (
            "gnome-terminal",
            &["--", "journalctl", "--user", "-u", "execlaw", "-f"],
        ),
        (
            "konsole",
            &["-e", "journalctl", "--user", "-u", "execlaw", "-f"],
        ),
        (
            "xfce4-terminal",
            &["-e", "journalctl --user -u execlaw -f"],
        ),
        ("xterm", &["-e", "journalctl --user -u execlaw -f"]),
    ];
    for (bin, args) in cmds {
        let spawn = Command::new(bin)
            .args(*args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawn.is_ok() {
            return;
        }
    }
    tracing::warn!("no terminal emulator found on PATH; journal viewer skipped");
}

/// Path to the bundled `execlaw` binary. Tauri's deb bundler places
/// `externalBin` siblings next to the main app binary at
/// `/usr/bin/execlaw`, and the tray is at `/usr/bin/execlaw-tray`,
/// so they share the same parent directory. The caller passes the
/// tray exe's parent rather than letting us re-derive it from
/// `std::env::current_exe()` so the unit test can construct a
/// fake layout.
fn bundled_execlaw_path(tray_exe_dir: &std::path::Path) -> std::path::PathBuf {
    tray_exe_dir.join("execlaw")
}

/// Convenience wrapper for callers that don't already have the tray
/// exe's directory cached. Resolves it from `std::env::current_exe()`.
/// Logs and falls back to `/usr/bin/` on failure (the conventional
/// location the .deb bundler ships into).
pub fn current_exe_dir() -> std::path::PathBuf {
    match std::env::current_exe() {
        Ok(path) => path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| {
                tracing::warn!(
                    "current_exe path {} has no parent dir; falling back to /usr/bin",
                    path.display()
                );
                std::path::PathBuf::from("/usr/bin")
            }),
        Err(e) => {
            tracing::warn!(error = %e, "current_exe failed; falling back to /usr/bin");
            std::path::PathBuf::from("/usr/bin")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_execlaw_path_is_sibling_of_tray() {
        // Whatever directory the caller passes, the bundled server
        // exe should be `<dir>/execlaw`. This is the contract with
        // Tauri's externalBin staging (Tauri strips the rustc triple
        // at bundle time so the final filename is just `execlaw`).
        let dir = std::path::Path::new("/usr/bin");
        let exe = bundled_execlaw_path(dir);
        assert_eq!(exe, std::path::PathBuf::from("/usr/bin/execlaw"));
    }

    #[test]
    fn from_is_active_maps_all_documented_states() {
        // The set of strings `systemctl is-active` emits is
        // documented in `systemctl(1)`; we cover every one.
        assert_eq!(UnitStatus::from_is_active("active"), UnitStatus::Running);
        assert_eq!(UnitStatus::from_is_active("inactive"), UnitStatus::Stopped);
        assert_eq!(UnitStatus::from_is_active("failed"), UnitStatus::Failed);
        assert_eq!(UnitStatus::from_is_active("activating"), UnitStatus::Pending);
        assert_eq!(UnitStatus::from_is_active("deactivating"), UnitStatus::Pending);
        assert_eq!(UnitStatus::from_is_active("reloading"), UnitStatus::Pending);
        assert_eq!(
            UnitStatus::from_is_active("unknown"),
            UnitStatus::NotInstalled
        );
        // Defensive: an unrecognised value (future systemd version
        // adds a state we don't know about) lands as Error rather
        // than misclassifying.
        assert_eq!(UnitStatus::from_is_active("xyzzy"), UnitStatus::Error);
    }

    #[test]
    fn from_is_active_trims_whitespace() {
        // `is-active` emits a trailing newline; the matcher must
        // tolerate it.
        assert_eq!(UnitStatus::from_is_active("active\n"), UnitStatus::Running);
        assert_eq!(UnitStatus::from_is_active("  failed  "), UnitStatus::Failed);
    }

}
