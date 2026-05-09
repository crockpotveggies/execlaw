//! Phase 14 — bare-metal service registration.
//!
//! The control plane runs as a long-lived OS service on the host.
//! `service-manager` abstracts the per-platform mechanics:
//!
//!   * Linux  → systemd unit (user-level by default; `--system` for root)
//!   * macOS  → launchd plist
//!   * Windows → Service Control Manager
//!
//! Subcommands:
//!
//!   * `execlaw service install [--system]` — register the service.
//!   * `execlaw service start` — start it.
//!   * `execlaw service stop` — stop it.
//!   * `execlaw service restart` — stop + start.
//!   * `execlaw service status` — query running state.
//!   * `execlaw service uninstall` — deregister.
//!
//! The hidden `execlaw service run` subcommand is what the service
//! unit invokes. On Linux/macOS it's a thin wrapper that calls
//! `serve` after a tiny stable-arg projection. On Windows it dispatches
//! into `windows-service`'s SCM event loop so the binary can answer
//! Stop/Pause/PowerEvent control messages.
//!
//! ## Why this lives in the CLI crate
//!
//! `service-manager` only matters at install time + when the service
//! itself dispatches its run loop. Both are CLI-layer concerns; the
//! `execlaw-server` crate stays free of OS-service plumbing so unit
//! tests don't need to mock any of it.

use crate::default_data_dir;
use anyhow::Context;
use service_manager::{
    ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx, ServiceStopCtx,
    ServiceUninstallCtx,
};
use std::ffi::OsString;
use std::path::PathBuf;

/// Stable identifier for the systemd unit / launchd label / Windows
/// SCM name. Matches the binary name so `journalctl -u execlaw`,
/// `launchctl list | grep execlaw`, and the Windows Services snap-in
/// all surface the same string.
pub const SERVICE_LABEL: &str = "execlaw";

/// Default bind address the installed service listens on. Loopback
/// only — operators put a reverse proxy in front if they want to
/// expose it on a LAN.
pub const SERVICE_BIND: &str = "127.0.0.1:3031";

fn label() -> ServiceLabel {
    SERVICE_LABEL
        .parse()
        .expect("SERVICE_LABEL must parse as a service label")
}

fn manager(system: bool) -> anyhow::Result<Box<dyn ServiceManager>> {
    let mut mgr =
        <dyn ServiceManager>::native().context("no native service manager available on this OS")?;
    // Windows SCM is system-only — there's no "user-level" service
    // concept on Windows. Coerce to System and emit a notice on
    // `install` so the operator knows their `--system` flag was
    // ignored (or implied). Linux + macOS honour the flag verbatim.
    let level = if cfg!(target_os = "windows") {
        if !system {
            let invoked_verb = std::env::args().nth(2).unwrap_or_default();
            if invoked_verb == "install" {
                eprintln!(
                    "NOTE: Windows always uses system-level service install \
                     (the SCM has no per-user mode). Requires an elevated \
                     PowerShell."
                );
            }
        }
        ServiceLevel::System
    } else if system {
        ServiceLevel::System
    } else {
        ServiceLevel::User
    };
    mgr.set_level(level)
        .with_context(|| format!("service manager doesn't support {level:?} level"))?;
    Ok(mgr)
}

/// Path to the running `execlaw` binary. The service unit invokes
/// the same path that ran `service install`, so a `cargo install
/// --force execlaw` upgrade picks up cleanly on next restart.
fn current_binary() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("could not locate execlaw binary path")
}

/// Build the args list the service unit will invoke. We use the
/// hidden `execlaw service run` subcommand so the binary can pick
/// the right OS-service dispatch on Windows.
///
/// The bind address is intentionally NOT baked into the unit — the
/// binary reads it from `config_general.bind_address` at boot, so
/// changes saved in Settings → General take effect on the next
/// `execlaw service restart` without needing to rewrite the unit.
fn service_run_args(db_path: &PathBuf) -> Vec<OsString> {
    vec![
        OsString::from("service"),
        OsString::from("run"),
        OsString::from("--db"),
        OsString::from(db_path),
    ]
}

/// Register the service with the host's service manager.
///
/// Idempotent against an already-installed unit: service-manager
/// returns success on a second install for systemd and launchd; on
/// Windows it returns a "service exists" error which we surface
/// verbatim. The operator-facing recovery is `execlaw service
/// uninstall` followed by re-install.
///
/// Permission failures (Windows SCM access denied, Linux/macOS root
/// requirement for system-level) are detected via the io::Error kind
/// and surfaced as a clear actionable message rather than a raw OS
/// errno.
pub fn install(system: bool, bind: Option<String>, db: Option<PathBuf>) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    let db_path = db.unwrap_or_else(|| default_data_dir().join("execlaw.db"));

    // If the operator passed `--bind`, persist it to
    // config_general.bind_address. The unit doesn't carry the bind
    // anymore — the binary reads it from the DB on every start, so
    // SPA edits and CLI install both flow through the same row.
    if let Some(b) = bind.as_deref() {
        write_bind_to_db(&db_path, b)
            .with_context(|| format!("save --bind={b} to {}", db_path.display()))?;
    }

    let program = current_binary()?;
    let args = service_run_args(&db_path);

    let ctx = ServiceInstallCtx {
        label: label(),
        program: program.clone(),
        args,
        contents: None,
        username: None,
        working_directory: Some(default_data_dir()),
        environment: None,
        autostart: true,
        disable_restart_on_failure: false,
    };
    if let Err(e) = mgr.install(ctx) {
        return Err(decorate_permission_error(e, system, "install"));
    }
    println!(
        "==> service installed: {} → {} ({} level)",
        SERVICE_LABEL,
        program.display(),
        if system { "system" } else { "user" }
    );
    if let Some(b) = bind.as_deref() {
        println!("    bind = {b} (saved to config_general)");
    } else {
        println!("    bind = (read from config_general at start)");
    }
    println!("    db   = {}", db_path.display());
    println!("    Use `execlaw service start` to launch.");
    Ok(())
}

/// Open the DB long enough to write `bind` to the
/// `config_general.bind_address` column, then close it. Used during
/// `service install --bind X` so the value persists across service
/// restarts and matches what Settings → General would write.
fn write_bind_to_db(db_path: &PathBuf, bind: &str) -> anyhow::Result<()> {
    use execlaw_core::general_settings::{GeneralSettingsStore, GeneralSettingsUpdate};
    let db =
        crate::open_db(db_path, false).with_context(|| format!("open {}", db_path.display()))?;
    execlaw_core::MigrationRunner::new(&db).apply_all()?;
    let store = GeneralSettingsStore::new(&db);
    store
        .update(
            &GeneralSettingsUpdate {
                start_on_boot: None,
                bind_address: Some(bind.to_owned()),
                setup_wizard_dismissed: None,
                history_retention_days: None,
            },
            chrono::Utc::now().timestamp(),
        )
        .map_err(|e| anyhow::anyhow!("save bind_address: {e}"))?;
    Ok(())
}

pub fn start(system: bool) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    if let Err(e) = mgr.start(ServiceStartCtx { label: label() }) {
        return Err(decorate_permission_error(e, system, "start"));
    }
    println!("==> service started: {SERVICE_LABEL}");
    Ok(())
}

pub fn stop(system: bool) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    if let Err(e) = mgr.stop(ServiceStopCtx { label: label() }) {
        return Err(decorate_permission_error(e, system, "stop"));
    }
    println!("==> service stopped: {SERVICE_LABEL}");
    Ok(())
}

pub fn restart(system: bool) -> anyhow::Result<()> {
    // Some platforms (notably systemd) expose a native restart;
    // service-manager doesn't, so we sequence stop → start. A
    // failed stop on a not-running service is downgraded to a
    // warning so `execlaw service restart` works as a "make sure
    // it's running" idempotent verb.
    let mgr = manager(system)?;
    if let Err(e) = mgr.stop(ServiceStopCtx { label: label() }) {
        // Permission errors on the stop step are still actionable —
        // the operator likely needs elevation for the start too. So
        // surface them with the decorator instead of swallowing.
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(decorate_permission_error(e, system, "restart"));
        }
        eprintln!("WARN: stop failed (probably not running): {e}");
    }
    if let Err(e) = mgr.start(ServiceStartCtx { label: label() }) {
        return Err(decorate_permission_error(e, system, "restart"));
    }
    println!("==> service restarted: {SERVICE_LABEL}");
    Ok(())
}

pub fn uninstall(system: bool) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    // Best-effort stop first so the uninstall doesn't race a
    // running instance.
    let _ = mgr.stop(ServiceStopCtx { label: label() });
    if let Err(e) = mgr.uninstall(ServiceUninstallCtx { label: label() }) {
        return Err(decorate_permission_error(e, system, "uninstall"));
    }
    println!("==> service uninstalled: {SERVICE_LABEL}");
    Ok(())
}

/// Print a best-effort status line. service-manager doesn't expose a
/// uniform status query (Linux/macOS would need shelling out to
/// `systemctl` / `launchctl`); we report install state via a
/// can-uninstall probe. Operators wanting deep status read
/// `journalctl -u execlaw` (Linux) / `Get-Service execlaw` (Windows)
/// / `launchctl list execlaw` (macOS) — printed in the help text.
pub fn status(system: bool) -> anyhow::Result<()> {
    let _mgr = manager(system)?;
    println!(
        "service `{SERVICE_LABEL}` ({} level) — for live status:",
        if system { "system" } else { "user" }
    );
    if cfg!(target_os = "linux") {
        let unit_scope = if system { "system" } else { "--user" };
        println!("  systemctl {unit_scope} status {SERVICE_LABEL}");
        println!("  journalctl {unit_scope} -u {SERVICE_LABEL} -f");
    } else if cfg!(target_os = "macos") {
        println!("  launchctl list | grep {SERVICE_LABEL}");
        println!("  log stream --predicate 'process == \"{SERVICE_LABEL}\"'");
    } else if cfg!(target_os = "windows") {
        println!("  Get-Service {SERVICE_LABEL}");
        println!("  Get-EventLog -Source {SERVICE_LABEL} -LogName Application");
    }
    Ok(())
}

/// Translate an `io::Error` from service-manager into an
/// operator-actionable message when the cause is missing privileges.
/// Otherwise pass the error through with the verb in context.
///
/// `service-manager` shells out to `sc.exe` (Windows), `systemctl`
/// (Linux), and `launchctl` (macOS) under the hood — it wraps the
/// child's stderr in an `io::Error` whose `kind()` is `Other`, so we
/// can't trust kind-based dispatch. Instead, we sniff the error
/// message for the platform-specific access-denied signature:
///
/// - Windows: `sc.exe` returns exit 5 + "Access is denied"
/// - Linux:   `systemctl` says "Access denied" or "EACCES"
/// - macOS:   `launchctl` returns "Permission denied" or
///   "Operation not permitted"
fn decorate_permission_error(err: std::io::Error, system: bool, verb: &str) -> anyhow::Error {
    let denied = is_access_denied(&err);
    if !denied {
        return anyhow::Error::new(err).context(format!("could not {verb} `{SERVICE_LABEL}`"));
    }
    let hint = if cfg!(target_os = "windows") {
        // Windows SCM is system-only; --system is implied.
        "Re-run from an elevated PowerShell (right-click → Run as Administrator). \
         The Service Control Manager refuses non-Administrator access."
    } else if system {
        "Re-run with `sudo` — system-level service registration writes to \
         a root-owned directory."
    } else {
        // Per-user service install on Linux/macOS shouldn't need root;
        // a permission error here means the user-level service dir is
        // unwritable for some other reason (umask, ACL, …).
        "Permission denied for the user-level service directory. \
         Check that the systemd / launchd user dir is writable, or \
         re-run with `--system` + `sudo` for a system-level install."
    };
    anyhow::Error::new(err).context(format!(
        "could not {verb} `{SERVICE_LABEL}`: access denied. {hint}"
    ))
}

/// Sniff an `io::Error` for the access-denied signature. Combines:
///
///   1. The standard `PermissionDenied` kind (set when service-manager
///      itself produces an `io::Error` from a syscall, e.g. for the
///      systemd user-dir mkdir path).
///   2. The Win32 `ERROR_ACCESS_DENIED = 5` raw OS error.
///   3. The Unix `EACCES = 13` raw OS error.
///   4. Sub-process stderr substrings — service-manager shells out to
///      `sc.exe` / `systemctl` / `launchctl` and wraps the child's
///      output in `io::Error::other`, where `kind()` is `Other` but
///      the message carries the access-denied text.
fn is_access_denied(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    match err.raw_os_error() {
        Some(5) | Some(13) => return true,
        _ => {}
    }
    // service-manager's sub-process errors put the child stderr in
    // either the top-level `Display` (single-line case) or as a
    // chained source (when the wrapper splits into "Command failed"
    // + "<stderr>"). Concatenate both so we don't miss the literal
    // "Access is denied." text from `sc.exe`.
    let mut combined = err.to_string().to_ascii_lowercase();
    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(err);
    while let Some(s) = source {
        combined.push_str(" | ");
        combined.push_str(&s.to_string().to_ascii_lowercase());
        source = s.source();
    }
    combined.contains("access is denied")
        || combined.contains("access denied")
        || combined.contains("permission denied")
        || combined.contains("operation not permitted")
        || combined.contains("exit code 5") // sc.exe ERROR_ACCESS_DENIED
}

// ---------------------------------------------------------------------------
// Service-side runtime: `execlaw service run`
// ---------------------------------------------------------------------------

/// Windows-only entry point invoked by the dispatcher branch in
/// `main.rs`. Wraps the SCM event loop. Returns when the SCM tells
/// us to stop OR when `cmd_serve` returns (e.g. fatal init error).
#[cfg(windows)]
pub fn windows_runtime_run(
    bind: Option<String>,
    db: PathBuf,
    no_encrypt: bool,
) -> anyhow::Result<()> {
    windows_runtime::run(bind, db, no_encrypt)
}

#[cfg(windows)]
mod windows_runtime {
    //! Windows SCM dispatch for the service runtime. `service-manager`
    //! handles install/start/stop from the host side; this module
    //! handles the in-process SCM event loop the SCM expects every
    //! Windows service to implement.

    use anyhow::Context;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Duration;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    /// Args captured from `execlaw service run` and consumed by the
    /// SCM-invoked entry point. Windows starts the service via
    /// `StartServiceCtrlDispatcher`, which means our entry point
    /// runs in a separate thread the SCM owns; the args are passed
    /// through a shared `OnceLock` instead of as fn parameters.
    struct ServiceArgs {
        bind: Option<String>,
        db: PathBuf,
        no_encrypt: bool,
    }

    static ARGS: OnceLock<ServiceArgs> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        // Errors here are reported to the SCM via SetServiceStatus;
        // there's no stderr the operator would see.
        let _ = run_service_event_loop();
    }

    fn run_service_event_loop() -> anyhow::Result<()> {
        let args = ARGS
            .get()
            .context("service args not initialized before SCM dispatch")?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let mut shutdown_tx = Some(shutdown_tx);

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Some(tx) = shutdown_tx.take() {
                        let _ = tx.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle =
            service_control_handler::register(crate::service::SERVICE_LABEL, event_handler)
                .context("register SCM control handler")?;

        // Tell the SCM we're running.
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        // Build a tokio runtime in this thread (SCM threads are not
        // tokio-aware) and run the server until shutdown_rx fires.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build tokio runtime for service")?;
        let result: anyhow::Result<()> = rt.block_on(async move {
            tokio::select! {
                r = crate::cmd_serve(args.bind.clone(), args.db.clone(), args.no_encrypt) => r,
                _ = shutdown_rx => Ok(()),
            }
        });

        // Tell the SCM we're stopping regardless of result; failures
        // surface in the Application event log via tracing.
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: match &result {
                Ok(()) => ServiceExitCode::Win32(0),
                Err(_) => ServiceExitCode::ServiceSpecific(1),
            },
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
        result
    }

    pub fn run(bind: Option<String>, db: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
        ARGS.set(ServiceArgs {
            bind,
            db,
            no_encrypt,
        })
        .map_err(|_| anyhow::anyhow!("service args already initialized"))?;
        // service_dispatcher::start blocks until the service exits
        // (SCM stop or process exit). On non-service invocations
        // (operator runs `execlaw service run` from a shell) it
        // returns an error; we fall back to running the server
        // directly so the same command works for ad-hoc dev too.
        match service_dispatcher::start(crate::service::SERVICE_LABEL, ffi_service_main) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    "windows SCM dispatcher unavailable ({e}); running server in foreground"
                );
                let args = ARGS.get().expect("ARGS just set above");
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(crate::cmd_serve(
                    args.bind.clone(),
                    args.db.clone(),
                    args.no_encrypt,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `decorate_permission_error` is the only function in this
    /// module worth unit-testing without a live SCM / systemd —
    /// everything else is a thin pass-through to service-manager.
    #[test]
    fn decorate_passes_through_non_permission_errors() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing unit");
        let err = decorate_permission_error(io_err, false, "stop");
        let msg = format!("{err:#}");
        // No elevation hint when the cause isn't permissions.
        assert!(!msg.contains("Administrator"));
        assert!(!msg.contains("sudo"));
        // The verb + label still appear so the user knows which
        // command failed.
        assert!(msg.contains("stop"));
        assert!(msg.contains(SERVICE_LABEL));
    }

    #[test]
    fn is_access_denied_matches_kind_and_raw_os_error() {
        // 1. ErrorKind::PermissionDenied — most direct signal.
        let e1 = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES");
        assert!(is_access_denied(&e1));
        // 2. Sub-process stderr text from sc.exe — kind is Other.
        let e2 = std::io::Error::other(
            "Command failed with exit code 5: [SC] OpenSCManager FAILED 5: \
             \r\n\r\nAccess is denied.",
        );
        assert!(is_access_denied(&e2));
        // 3. Stderr text from systemctl.
        let e3 = std::io::Error::other("Failed to start execlaw.service: Access denied");
        assert!(is_access_denied(&e3));
        // 4. macOS launchctl text.
        let e4 =
            std::io::Error::other("/bin/launchctl bootstrap returned: Operation not permitted");
        assert!(is_access_denied(&e4));
        // 5. Generic "not found" — doesn't match.
        let e5 = std::io::Error::new(std::io::ErrorKind::NotFound, "service not registered");
        assert!(!is_access_denied(&e5));
    }

    #[test]
    fn decorate_recognizes_sc_exe_access_denied_substring() {
        // The Windows live-test failure mode: service-manager shells
        // out to `sc.exe`, the SCM rejects with code 5, the err is
        // an io::Error::other with the stderr concatenated. Our
        // decorator must pick this up.
        let io_err = std::io::Error::other(
            "Command failed with exit code 5: [SC] OpenSCManager FAILED 5: \
             \r\n\r\nAccess is denied.",
        );
        let err = decorate_permission_error(io_err, false, "install");
        let msg = format!("{err:#}");
        if cfg!(target_os = "windows") {
            assert!(
                msg.contains("Administrator") || msg.contains("elevated"),
                "Windows path must include the Administrator hint; got: {msg}"
            );
        } else {
            // On non-Windows, the same error text still triggers
            // the access-denied branch but the hint is the
            // platform-specific one.
            assert!(
                msg.contains("user-level service directory") || msg.contains("sudo"),
                "non-Windows path must include the dir/sudo hint; got: {msg}"
            );
        }
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn decorate_permission_denied_includes_admin_hint_on_windows() {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access is denied. (os error 5)",
        );
        let err = decorate_permission_error(io_err, false, "install");
        let msg = format!("{err:#}");
        assert!(msg.contains("elevated PowerShell"));
        assert!(msg.contains("Administrator"));
    }

    #[test]
    #[cfg(unix)]
    fn decorate_permission_denied_with_system_includes_sudo_hint() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES");
        let err = decorate_permission_error(io_err, true, "install");
        let msg = format!("{err:#}");
        assert!(msg.contains("sudo"));
    }

    #[test]
    #[cfg(unix)]
    fn decorate_permission_denied_user_level_includes_dir_hint() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES");
        let err = decorate_permission_error(io_err, false, "install");
        let msg = format!("{err:#}");
        // User-level install hint should mention the writable check
        // rather than promising sudo will fix it.
        assert!(msg.contains("user-level service directory"));
    }
}
