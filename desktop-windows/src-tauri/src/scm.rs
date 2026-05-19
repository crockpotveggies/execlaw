//! Thin Rust wrapper over Windows' Service Control Manager (SCM).
//!
//! The Windows analogue of `desktop-macos/src-tauri/src/sm_app_service.rs`.
//! Whereas macOS uses `SMAppService` to register a bundled LaunchAgent
//! plist (which auto-cleans on bundle removal), Windows uses the SCM
//! and the install / uninstall happens at NSIS install-time via
//! `installer/hooks.nsh`, which shells out to the bundled
//! `execlaw.exe service install` / `service uninstall`. That same
//! `execlaw.exe` is what the SCM later invokes via
//! `execlaw service run` (see `crates/cli/src/service.rs` —
//! `windows_runtime` handles the SCM event-loop dance).
//!
//! This module only needs to:
//!
//!   * Query the install + run state of the `execlaw` service so the
//!     tray status row can say "Service: Running / Stopped / Not
//!     installed / …".
//!   * Re-launch the bundled `execlaw.exe` with the `runas` verb to
//!     elevate when the operator clicks "Restart service" (SCM
//!     control verbs need Administrator).
//!   * Open the Services MMC snap-in (`services.msc`) so the
//!     operator can drive the service manually if anything in our
//!     UI fails.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// Stable identifier for the SCM service. Must match
/// `crates/cli/src/service.rs::SERVICE_LABEL`. If that constant ever
/// changes, this must change in lockstep.
pub const SERVICE_LABEL: &str = "execlaw";

/// Tray-side view of the SCM service. Mirrors the macOS
/// `AgentStatus` enum shape so `compose_status_label` can use the
/// same match arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcStatus {
    /// The service is not installed at all — the NSIS post-install
    /// hook either hasn't run or was uninstalled. Tray surfaces this
    /// as an actionable row ("Reinstall via the .exe installer").
    NotInstalled,
    /// Installed and running.
    Running,
    /// Installed but stopped (manual / boot failure / explicit stop).
    Stopped,
    /// In a transient state (StartPending / StopPending /
    /// ContinuePending / PausePending).
    Pending,
    /// Installed and Paused — uncommon for our service but the SCM
    /// state machine includes it. Surfaced for completeness.
    Paused,
    /// Couldn't talk to the SCM at all (open_manager failed, e.g. no
    /// SC_MANAGER_CONNECT rights). Tray shows the error string in
    /// the row so the operator can act.
    Error,
}

impl SvcStatus {
    /// Translate a `windows_service::service::ServiceState` into our
    /// coarser tray-side enum. We collapse the four *Pending states
    /// into a single bucket because the tray label can't usefully
    /// distinguish them at 5-second poll intervals.
    fn from_state(s: ServiceState) -> Self {
        match s {
            ServiceState::Stopped => Self::Stopped,
            ServiceState::StartPending
            | ServiceState::StopPending
            | ServiceState::ContinuePending
            | ServiceState::PausePending => Self::Pending,
            ServiceState::Running => Self::Running,
            ServiceState::Paused => Self::Paused,
        }
    }
}

/// Open the SCM with the minimum privilege we need to query a named
/// service. Querying does NOT require Administrator on modern
/// Windows (Vista+) — every authenticated user gets
/// `SC_MANAGER_CONNECT` by default. Modifying state (Stop / Start /
/// Restart) DOES need elevation; we handle that out-of-band via the
/// `runas` shell verb (see `restart_service_elevated`).
fn open_manager_for_query() -> Result<ServiceManager, ScmError> {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(ScmError::from_windows_service)
}

/// Errors bubbling up from the SCM. We don't differentiate "not
/// installed" from "access denied" at the enum level — both surface
/// to the operator as a single Error variant with a string; the
/// `query()` function disambiguates the not-installed case before
/// constructing one.
#[derive(Debug, thiserror::Error)]
pub enum ScmError {
    #[error("SCM error: {message} (Win32 code {code})")]
    Win32 { code: i32, message: String },
    #[error("SCM error: {0}")]
    Other(String),
}

impl ScmError {
    fn from_windows_service(err: windows_service::Error) -> Self {
        match err {
            windows_service::Error::Winapi(io) => Self::Win32 {
                code: io.raw_os_error().unwrap_or(0),
                message: io.to_string(),
            },
            other => Self::Other(other.to_string()),
        }
    }
}

/// Query the install + run state of the `execlaw` service in one
/// shot. Returns `SvcStatus::NotInstalled` for the explicit
/// "ERROR_SERVICE_DOES_NOT_EXIST" (1060) case so the tray can show
/// an actionable row, vs. `SvcStatus::Error` for everything else.
pub fn query() -> SvcStatus {
    // `ERROR_SERVICE_DOES_NOT_EXIST` — the public win32 constant
    // value for "the SCM has never heard of a service by that
    // name". The `windows-service` crate wraps the OpenServiceW
    // call's GetLastError() in an `io::Error`, so we read
    // `raw_os_error()` to disambiguate.
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

    let manager = match open_manager_for_query() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "SCM open_manager_for_query failed");
            return SvcStatus::Error;
        }
    };
    let service = match manager.open_service(SERVICE_LABEL, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(io))
            if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            return SvcStatus::NotInstalled;
        }
        Err(e) => {
            tracing::warn!(error = ?e, "SCM open_service('{SERVICE_LABEL}') failed");
            return SvcStatus::Error;
        }
    };
    match service.query_status() {
        Ok(status) => SvcStatus::from_state(status.current_state),
        Err(e) => {
            tracing::warn!(error = ?e, "SCM query_status failed");
            SvcStatus::Error
        }
    }
}

/// Whether the tray should expose a "Reinstall via .exe installer"
/// action. True only when the SCM reports the service is missing —
/// which means the operator either uninstalled it manually with
/// `sc.exe delete` or ran the NSIS uninstaller and is now relaunching
/// a stale tray.
pub fn requires_install_action(s: SvcStatus) -> bool {
    matches!(s, SvcStatus::NotInstalled)
}

/// Open the Windows Services MMC snap-in. Best-effort — used as a
/// last-resort "drive the service yourself" affordance.
///
/// `services.msc` is opened via the file association (mmc.exe is
/// registered as the handler). On a stock Windows install this
/// always succeeds; we log + ignore failures because the worst case
/// is the operator falls back to running `services.msc` from the
/// Start menu themselves.
pub fn open_services_mmc() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb: Vec<u16> = OsString::from("open")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let file: Vec<u16> = OsString::from("services.msc")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: All wide strings are NUL-terminated owned buffers
    // whose lifetimes outlive the call. ShellExecuteW is documented
    // as thread-safe; we call it from the menu-event thread (the
    // Tauri main thread). The HINSTANCE return value is opaque — we
    // only care whether it's <= 32 (failure).
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (rc as isize) <= 32 {
        tracing::warn!(rc = rc as isize, "ShellExecuteW(services.msc) failed");
    }
}

/// Trigger an elevated restart of the SCM service.
///
/// SCM control verbs (`Stop`, `Start`) require Administrator on the
/// `execlaw` service because we registered it at perMachine scope
/// via the NSIS installer. The tray itself runs as the logged-in
/// (non-admin) user, so we can't drive `service::restart()` directly
/// in-process — Windows would return ERROR_ACCESS_DENIED.
///
/// Instead we re-launch the bundled `execlaw.exe` with the `runas`
/// shell verb, which makes the shell invoke UAC consent before
/// spawning the elevated child. The child runs `service restart`,
/// reports the result on its own stdout (which the operator doesn't
/// see — the tray polls SCM state to confirm), and exits.
///
/// Returns `Ok(())` once the spawn is scheduled — we don't block on
/// the child completing; the SCM poller picks up the new state on
/// its next 5-second tick.
pub fn restart_service_elevated(tray_exe_dir: &Path) -> Result<(), ScmError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let bundled_exe = bundled_execlaw_path(tray_exe_dir);
    if !bundled_exe.exists() {
        return Err(ScmError::Other(format!(
            "bundled `execlaw.exe` not found next to the tray binary \
             (looked for {}). Reinstall via the .exe installer.",
            bundled_exe.display()
        )));
    }

    // Wide-string everything ShellExecuteW touches. The verb
    // `"runas"` is what makes Windows trigger the UAC consent prompt.
    let verb: Vec<u16> = OsString::from("runas")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let file: Vec<u16> = bundled_exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let params: Vec<u16> = OsString::from("service restart")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: Buffer lifetimes cover the call; ShellExecuteW is
    // documented as safe to invoke off any thread once the COM
    // apartment is initialised, which Tauri does during `Builder::
    // build()`. SW_HIDE keeps the elevated `execlaw.exe service
    // restart` invisible — it's a fire-and-forget elevated helper.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        )
    };
    if (rc as isize) <= 32 {
        // 1223 = ERROR_CANCELLED — operator clicked Cancel on UAC.
        // Surface that as a softer error so we don't show a scary
        // dialog when the operator simply changed their mind.
        if (rc as isize) == 1223 {
            return Err(ScmError::Other(
                "Elevation request cancelled (UAC declined).".to_string(),
            ));
        }
        return Err(ScmError::Win32 {
            code: rc as i32,
            message: "ShellExecuteW(runas execlaw.exe) failed".to_string(),
        });
    }
    Ok(())
}

/// Trigger an elevated uninstall of the SCM service (without
/// removing the .exe / Start Menu shortcuts — the uninstaller's job).
/// Used by the tray's "Uninstall execlaw…" menu item for parity with
/// the macOS "Uninstall execlaw…" flow.
///
/// In the common case the operator will use *Settings → Apps →
/// execlaw → Uninstall* instead, which runs the NSIS uninstaller and
/// the matching `pre-uninstall` hook that calls `service stop` +
/// `service uninstall` for us. This menu item exists for the rarer
/// path where the operator wants to deregister the service without
/// removing the installed program.
pub fn uninstall_service_elevated(tray_exe_dir: &Path) -> Result<(), ScmError> {
    run_bundled_command_elevated(tray_exe_dir, "service uninstall")
}

fn run_bundled_command_elevated(tray_exe_dir: &Path, args: &str) -> Result<(), ScmError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let bundled_exe = bundled_execlaw_path(tray_exe_dir);
    if !bundled_exe.exists() {
        return Err(ScmError::Other(format!(
            "bundled `execlaw.exe` not found next to the tray binary \
             (looked for {}).",
            bundled_exe.display()
        )));
    }
    let verb: Vec<u16> = OsString::from("runas")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let file: Vec<u16> = bundled_exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let params: Vec<u16> = OsString::from(args)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: same as `restart_service_elevated`.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        )
    };
    if (rc as isize) <= 32 {
        if (rc as isize) == 1223 {
            return Err(ScmError::Other(
                "Elevation request cancelled (UAC declined).".to_string(),
            ));
        }
        return Err(ScmError::Win32 {
            code: rc as i32,
            message: format!("ShellExecuteW(runas execlaw.exe {args}) failed"),
        });
    }
    Ok(())
}

/// Path to the bundled `execlaw.exe` (the server sidecar). Tauri's
/// NSIS bundler places `externalBin` siblings next to the main app
/// binary at `<INSTDIR>\execlaw.exe`, and the tray is at
/// `<INSTDIR>\execlaw-tray.exe`, so we look one dir up from the tray
/// binary's parent and tack on `execlaw.exe`. The caller passes the
/// tray exe's parent directory rather than letting us re-derive it
/// from `std::env::current_exe()` so the unit test can construct a
/// fake layout.
fn bundled_execlaw_path(tray_exe_dir: &Path) -> PathBuf {
    tray_exe_dir.join("execlaw.exe")
}

/// Convenience wrapper for callers that don't already have the tray
/// exe's directory cached. Resolves it from `std::env::current_exe()`.
/// Logs and falls back to the current working directory on failure.
pub fn current_exe_dir() -> PathBuf {
    match std::env::current_exe() {
        Ok(path) => path.parent().map(Path::to_path_buf).unwrap_or_else(|| {
            tracing::warn!(
                "current_exe path {} has no parent dir; falling back to cwd",
                path.display()
            );
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }),
        Err(e) => {
            tracing::warn!(error = %e, "current_exe failed; falling back to cwd");
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_execlaw_path_is_sibling_of_tray() {
        // Whatever directory the caller passes, the bundled server
        // exe should be `<dir>\execlaw.exe`. This is the contract
        // with Tauri's externalBin staging (Tauri strips the rustc
        // triple at bundle time so the final filename is just
        // `execlaw.exe`).
        let dir = Path::new("C:\\Program Files\\execlaw");
        let exe = bundled_execlaw_path(dir);
        assert_eq!(exe, PathBuf::from("C:\\Program Files\\execlaw\\execlaw.exe"));
    }

    #[test]
    fn from_state_collapses_pending_variants() {
        // Three of the four *Pending states must map to Pending, and
        // the four base states must map to themselves. Catches the
        // refactor hazard where the SCM enum grows a new variant we
        // haven't accounted for.
        assert_eq!(SvcStatus::from_state(ServiceState::Stopped), SvcStatus::Stopped);
        assert_eq!(SvcStatus::from_state(ServiceState::Running), SvcStatus::Running);
        assert_eq!(SvcStatus::from_state(ServiceState::Paused), SvcStatus::Paused);
        assert_eq!(
            SvcStatus::from_state(ServiceState::StartPending),
            SvcStatus::Pending
        );
        assert_eq!(
            SvcStatus::from_state(ServiceState::StopPending),
            SvcStatus::Pending
        );
        assert_eq!(
            SvcStatus::from_state(ServiceState::ContinuePending),
            SvcStatus::Pending
        );
        assert_eq!(
            SvcStatus::from_state(ServiceState::PausePending),
            SvcStatus::Pending
        );
    }

    #[test]
    fn requires_install_action_only_for_not_installed() {
        assert!(requires_install_action(SvcStatus::NotInstalled));
        assert!(!requires_install_action(SvcStatus::Running));
        assert!(!requires_install_action(SvcStatus::Stopped));
        assert!(!requires_install_action(SvcStatus::Pending));
        assert!(!requires_install_action(SvcStatus::Paused));
        assert!(!requires_install_action(SvcStatus::Error));
    }
}
