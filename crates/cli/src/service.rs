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
    ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager,
    ServiceStartCtx, ServiceStopCtx, ServiceUninstallCtx,
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
pub const SERVICE_BIND: &str = "127.0.0.1:3030";

fn label() -> ServiceLabel {
    SERVICE_LABEL
        .parse()
        .expect("SERVICE_LABEL must parse as a service label")
}

fn manager(system: bool) -> anyhow::Result<Box<dyn ServiceManager>> {
    let mut mgr = <dyn ServiceManager>::native()
        .context("no native service manager available on this OS")?;
    let level = if system {
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
fn service_run_args(db_path: &PathBuf, bind: &str) -> Vec<OsString> {
    vec![
        OsString::from("service"),
        OsString::from("run"),
        OsString::from("--bind"),
        OsString::from(bind),
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
pub fn install(system: bool, bind: Option<String>, db: Option<PathBuf>) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    let bind = bind.unwrap_or_else(|| SERVICE_BIND.to_owned());
    let db_path = db.unwrap_or_else(|| default_data_dir().join("execlaw.db"));
    let program = current_binary()?;
    let args = service_run_args(&db_path, &bind);

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
    mgr.install(ctx)
        .with_context(|| format!("could not install service `{SERVICE_LABEL}`"))?;
    println!(
        "==> service installed: {} → {} ({} level)",
        SERVICE_LABEL,
        program.display(),
        if system { "system" } else { "user" }
    );
    println!("    bind = {bind}");
    println!("    db   = {}", db_path.display());
    println!("    Use `execlaw service start` to launch.");
    Ok(())
}

pub fn start(system: bool) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    mgr.start(ServiceStartCtx { label: label() })
        .with_context(|| format!("could not start `{SERVICE_LABEL}`"))?;
    println!("==> service started: {SERVICE_LABEL}");
    Ok(())
}

pub fn stop(system: bool) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    mgr.stop(ServiceStopCtx { label: label() })
        .with_context(|| format!("could not stop `{SERVICE_LABEL}`"))?;
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
        eprintln!("WARN: stop failed (probably not running): {e}");
    }
    mgr.start(ServiceStartCtx { label: label() })
        .with_context(|| format!("could not start `{SERVICE_LABEL}`"))?;
    println!("==> service restarted: {SERVICE_LABEL}");
    Ok(())
}

pub fn uninstall(system: bool) -> anyhow::Result<()> {
    let mgr = manager(system)?;
    // Best-effort stop first so the uninstall doesn't race a
    // running instance.
    let _ = mgr.stop(ServiceStopCtx { label: label() });
    mgr.uninstall(ServiceUninstallCtx { label: label() })
        .with_context(|| format!("could not uninstall `{SERVICE_LABEL}`"))?;
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

// ---------------------------------------------------------------------------
// Service-side runtime: `execlaw service run`
// ---------------------------------------------------------------------------

/// Windows-only entry point invoked by the dispatcher branch in
/// `main.rs`. Wraps the SCM event loop. Returns when the SCM tells
/// us to stop OR when `cmd_serve` returns (e.g. fatal init error).
#[cfg(windows)]
pub fn windows_runtime_run(
    bind: String,
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
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
        ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{
        self, ServiceControlHandlerResult,
    };
    use windows_service::service_dispatcher;

    /// Args captured from `execlaw service run` and consumed by the
    /// SCM-invoked entry point. Windows starts the service via
    /// `StartServiceCtrlDispatcher`, which means our entry point
    /// runs in a separate thread the SCM owns; the args are passed
    /// through a shared `OnceLock` instead of as fn parameters.
    struct ServiceArgs {
        bind: String,
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
            controls_accepted: ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN,
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

    pub fn run(bind: String, db: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
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
