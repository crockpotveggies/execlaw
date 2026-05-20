//! Tauri app wiring — tray, menu, window, status polling, and the
//! handful of click handlers that map to SCM control calls.
//!
//! Lifetime model:
//!
//! - The NSIS installer registers the `execlaw` service via the
//!   bundled `execlaw.exe service install --system` call in its
//!   post-install hook (`installer/hooks.nsh`). The service runs as
//!   `LocalSystem` and starts at boot. Quitting the tray DOES NOT
//!   stop the service.
//! - "Uninstall execlaw" from the *Apps* control panel runs the NSIS
//!   uninstaller, whose pre-uninstall hook calls
//!   `execlaw.exe service stop` + `service uninstall` before removing
//!   the program files. That's the normal teardown path.
//! - The tray's *Uninstall execlaw…* menu item exists for the rarer
//!   case where the operator wants to deregister the service without
//!   removing the installed program (e.g. moving to a manual
//!   per-user install).
//!
//! This file mirrors `desktop-macos/src-tauri/src/app.rs` — the menu
//! item IDs, status-row composition, and double-click semantics are
//! deliberately identical so a Windows operator and a macOS operator
//! see the same UX. Differences are localised to:
//!
//!   * Tray icon source (`.ico` instead of a template `.png`).
//!   * No `ActivationPolicy` toggle (Windows doesn't have the
//!     Accessory/Regular distinction — opening a window doesn't
//!     promote us into anything).
//!   * "Open data folder" reveals `%USERPROFILE%\.execlaw`, the
//!     install-time admin's profile dir, since that's where the
//!     SCM-installed service writes the DB.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use std::time::Duration;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;

use crate::scm::{self, SvcStatus};
use crate::server_status::{self, ServerStatus};

/// Loopback URL the SPA + API both live behind. Hardcoded to the
/// `SERVICE_BIND` constant the Rust CLI uses (`crates/cli/src/
/// service.rs::SERVICE_BIND`). If that constant ever changes, this
/// must change in lockstep.
const SERVER_URL: &str = "http://127.0.0.1:3031";

/// Menu item IDs — kept as constants so the click dispatcher can
/// pattern-match without typo risk. Names mirror the macOS sibling
/// so future cross-platform refactors don't need to translate.
mod menu_ids {
    pub const STATUS: &str = "status";
    pub const OPEN: &str = "open";
    pub const SERVICES_MMC: &str = "services_mmc";
    pub const RESTART: &str = "restart";
    pub const DATA_FOLDER: &str = "data_folder";
    pub const UNINSTALL: &str = "uninstall";
    pub const QUIT: &str = "quit";
}

/// Tray menu items we need to mutate after construction. `status` is
/// the always-visible row; `services_mmc` is the elevated-action row
/// that's only enabled when the service is missing or errored.
struct MenuHandles {
    status: MenuItem<tauri::Wry>,
    services_mmc: MenuItem<tauri::Wry>,
}

/// Shared state stored in `app.manage()` so background tasks +
/// menu-event handlers can both access it.
struct TrayState {
    menu: Mutex<MenuHandles>,
    http: reqwest::Client,
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Build menu shell + tray. The status row is added first
            // as a disabled item ("Service: …") so it acts as a label.
            let status = MenuItem::with_id(
                app,
                menu_ids::STATUS,
                "Service: starting…",
                false,
                None::<&str>,
            )?;
            let services_mmc = MenuItem::with_id(
                app,
                menu_ids::SERVICES_MMC,
                "Open Services (services.msc)…",
                true,
                None::<&str>,
            )?;
            let open = MenuItem::with_id(
                app,
                menu_ids::OPEN,
                "Open execlaw",
                true,
                None::<&str>,
            )?;
            let restart = MenuItem::with_id(
                app,
                menu_ids::RESTART,
                "Restart service",
                true,
                None::<&str>,
            )?;
            let data_folder = MenuItem::with_id(
                app,
                menu_ids::DATA_FOLDER,
                "Open data folder",
                true,
                None::<&str>,
            )?;
            let uninstall = MenuItem::with_id(
                app,
                menu_ids::UNINSTALL,
                "Uninstall execlaw…",
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(
                app,
                menu_ids::QUIT,
                "Quit (leaves service running)",
                true,
                None::<&str>,
            )?;

            let menu = Menu::with_items(
                app,
                &[
                    &status,
                    &services_mmc,
                    &PredefinedMenuItem::separator(app)?,
                    &open,
                    &restart,
                    &PredefinedMenuItem::separator(app)?,
                    &data_folder,
                    &uninstall,
                    &PredefinedMenuItem::separator(app)?,
                    &quit,
                ],
            )?;

            // Hide the services.msc row by default — the SCM poller
            // re-enables it only when the service is missing or
            // errored. We mirror the macOS Approve-row pattern of
            // toggling enabled state + rewriting the label rather
            // than removing/re-adding items.
            services_mmc.set_enabled(false)?;
            services_mmc.set_text("(service OK)")?;

            let state = TrayState {
                menu: Mutex::new(MenuHandles {
                    status: status.clone(),
                    services_mmc: services_mmc.clone(),
                }),
                http: reqwest::Client::builder()
                    .user_agent("execlaw-tray-win/0.1")
                    .build()
                    .expect("reqwest client builds with default config"),
            };
            app.manage(Arc::new(state));

            // Tray icon — embed the .ico at compile time so it ships
            // in the binary, no I/O at boot. Windows uses .ico
            // (multi-resolution) and does NOT have macOS's template
            // image concept; the same icon shows on both light- and
            // dark-mode taskbars unchanged.
            let tray_image = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.ico"))
                .unwrap_or_else(|_| tauri::image::Image::new_owned(vec![0u8; 4], 1, 1));
            let _tray = TrayIconBuilder::with_id("execlaw-tray")
                .menu(&menu)
                .menu_on_left_click(true)
                .icon(tray_image)
                .tooltip("execlaw")
                .on_menu_event(handle_menu_event)
                .on_tray_icon_event(|_tray, event| {
                    // Left-click already opens the menu via
                    // `menu_on_left_click(true)`. Double-click could
                    // open the chat as a shortcut in a future
                    // revision — wired as a no-op for now.
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        // intentional no-op
                    }
                })
                .build(app)?;

            // Spawn the status poller. Polls every 5s — same cadence
            // as the macOS sibling so the user experience matches.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                run_status_poller(app_handle).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Don't quit when the last window closes — we're a tray
            // app, the notification-area icon is the only persistent
            // UI. (Windows has no Dock policy to flip on the way out
            // — the macOS Accessory/Regular dance has no analogue
            // here.)
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                api.prevent_exit();
            }
        });
}

/// Dispatch menu clicks to the right handler.
fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        menu_ids::OPEN => open_chat_window(app),
        menu_ids::SERVICES_MMC => scm::open_services_mmc(),
        menu_ids::RESTART => restart_service(app),
        menu_ids::DATA_FOLDER => open_data_folder(app),
        menu_ids::UNINSTALL => uninstall_flow(app),
        menu_ids::QUIT => {
            // Quitting the tray DOES NOT stop the SCM service.
            // That's the contract — `Get-Service execlaw` will still
            // show Running afterwards.
            app.exit(0);
        }
        // Status row is disabled, shouldn't fire — silent on any
        // unknown id.
        _ => {}
    }
}

/// Open (or focus) the chat UI window pointed at the local server.
///
/// Unlike the macOS sibling, no activation-policy toggle is needed —
/// Tauri's WebviewWindow under WebView2 surfaces above other windows
/// via the standard Win32 z-order mechanics. We DO call
/// `unminimize()` + `set_focus()` so a previously-minimised chat
/// window comes back to the foreground when the operator clicks
/// "Open execlaw" a second time.
fn open_chat_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("chat") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let url = WebviewUrl::External(
        SERVER_URL
            .parse()
            .expect("hardcoded server URL is a valid URI"),
    );
    let build = WebviewWindowBuilder::new(app, "chat", url)
        .title("execlaw")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        .resizable(true);
    match build.build() {
        Ok(window) => {
            let _ = window.set_focus();
        }
        Err(e) => tracing::error!(error = %e, "failed to open chat window"),
    }
}

/// Reveal `%USERPROFILE%\.execlaw` in Explorer. Matches the path the
/// SCM-installed service uses for its DB (we install with a `--db`
/// override pointing at the install-time admin's profile — see
/// `installer/hooks.nsh`). Creates the dir first if it doesn't exist
/// so the reveal always lands somewhere reasonable on a fresh
/// install.
fn open_data_folder(app: &AppHandle) {
    let Some(profile) = userprofile() else {
        tracing::warn!("could not resolve %USERPROFILE% for data folder reveal");
        return;
    };
    let data_dir = profile.join(".execlaw");
    let _ = std::fs::create_dir_all(&data_dir);
    if let Err(e) = app
        .opener()
        .reveal_item_in_dir(data_dir.to_string_lossy().as_ref())
    {
        tracing::warn!(error = %e, "open data folder failed");
    }
}

/// Resolve the user's profile directory. `%USERPROFILE%` is set on
/// every interactive Windows session; the tray inherits it.
fn userprofile() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
}

/// Restart the SCM service. Requires elevation — the call goes
/// through `scm::restart_service_elevated`, which re-launches the
/// bundled `execlaw.exe` with the `runas` shell verb so UAC fires.
fn restart_service(app: &AppHandle) {
    let app_for_dialog = app.clone();
    tauri::async_runtime::spawn(async move {
        let dir = scm::current_exe_dir();
        if let Err(e) = scm::restart_service_elevated(&dir) {
            show_error(
                &app_for_dialog,
                "Restart failed",
                &format!("Could not restart the execlaw service: {e}"),
            );
        }
    });
}

/// Uninstall flow. Two confirmation gates:
///   1. "Stop and remove the execlaw service?"
///   2. "Also delete your data?" — the destructive one.
///
/// This is the in-tray counterpart to *Settings → Apps → execlaw →
/// Uninstall*; both paths converge on `execlaw service uninstall`,
/// which deregisters the SCM entry.
fn uninstall_flow(app: &AppHandle) {
    let app_for_async = app.clone();
    tauri::async_runtime::spawn(async move {
        let confirmed = app_for_async
            .dialog()
            .message(
                "This stops the execlaw background service and removes its Windows \
                 service registration. Your data at %USERPROFILE%\\.execlaw\\ is \
                 preserved unless you also confirm deletion in the next step.\n\n\
                 Note: this does NOT remove the installed program — use \
                 Settings → Apps → execlaw → Uninstall for a full removal.\n\n\
                 Proceed?",
            )
            .title("Uninstall execlaw service")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Uninstall".to_string(),
                "Cancel".to_string(),
            ))
            .blocking_show();
        if !confirmed {
            return;
        }
        let dir = scm::current_exe_dir();
        if let Err(e) = scm::uninstall_service_elevated(&dir) {
            show_error(
                &app_for_async,
                "Uninstall failed",
                &format!("Could not unregister the execlaw service: {e}"),
            );
            return;
        }

        let delete_data = app_for_async
            .dialog()
            .message(
                "Also delete your data at %USERPROFILE%\\.execlaw\\?\n\n\
                 This permanently removes your encrypted vault, conversation history, \
                 plugin state, and event log. This cannot be undone.",
            )
            .title("Delete execlaw data?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Delete data".to_string(),
                "Keep data".to_string(),
            ))
            .blocking_show();
        if delete_data {
            if let Some(profile) = userprofile() {
                let data_dir = profile.join(".execlaw");
                if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                    show_error(
                        &app_for_async,
                        "Data deletion failed",
                        &format!("Could not remove {}: {e}", data_dir.display()),
                    );
                }
            }
        }

        app_for_async
            .dialog()
            .message(
                "execlaw service has been removed. To finish uninstalling the \
                 application, open Settings → Apps → execlaw → Uninstall.",
            )
            .title("Uninstall complete")
            .kind(MessageDialogKind::Info)
            .blocking_show();

        app_for_async.exit(0);
    });
}

fn show_error(app: &AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Error)
        .blocking_show();
}

/// Poll the server every 5s and rewrite the status row + the
/// services.msc-fallback row based on the result + SCM status.
async fn run_status_poller(app: AppHandle) {
    let state = app.state::<Arc<TrayState>>().inner().clone();
    loop {
        let server = server_status::probe(&state.http, SERVER_URL).await;
        let svc = scm::query();

        let label = compose_status_label(server, svc);
        let needs_mmc = scm::requires_install_action(svc) || matches!(svc, SvcStatus::Error);

        // Hold the lock only across the (synchronous) menu mutation
        // calls so we don't serialise the 5s poll behind UI work.
        let menu = state.menu.lock().await;
        if let Err(e) = menu.status.set_text(&label) {
            tracing::warn!(error = %e, "status item set_text failed");
        }
        if needs_mmc {
            let _ = menu.services_mmc.set_enabled(true);
            let _ = menu
                .services_mmc
                .set_text("Open Services (services.msc)…");
        } else {
            let _ = menu.services_mmc.set_enabled(false);
            let _ = menu.services_mmc.set_text("(service OK)");
        }
        drop(menu);

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Compose the tray status row. Combines the server-side health probe
/// with the SCM service state, so the user can distinguish "the
/// service isn't installed" from "the service is installed but the
/// server hasn't come up yet" from "the service is paused".
fn compose_status_label(server: ServerStatus, svc: SvcStatus) -> String {
    match (svc, server) {
        (SvcStatus::NotInstalled, _) => {
            "Service: not installed — reinstall via the .exe installer".into()
        }
        (SvcStatus::Error, _) => "Service: SCM query failed".into(),
        (SvcStatus::Paused, _) => "Service: paused".into(),
        (SvcStatus::Pending, _) => "Service: starting…".into(),
        (SvcStatus::Stopped, _) => "Service: Stopped".into(),
        // SCM says Running — defer to the HTTP probe for the
        // user-facing wording so first-run wizard states surface.
        (SvcStatus::Running, s) => format!("Service: {}", s.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_label_promotes_not_installed_over_server_state() {
        // Even if the server somehow answers a probe (it can't, but
        // hypothetically), the user-actionable condition is the
        // missing SCM entry — surface that.
        let label = compose_status_label(ServerStatus::Stopped, SvcStatus::NotInstalled);
        assert!(label.contains("not installed"));
    }

    #[test]
    fn compose_label_uses_server_state_when_service_running() {
        let label = compose_status_label(ServerStatus::Running, SvcStatus::Running);
        assert_eq!(label, "Service: Running");
        let label = compose_status_label(ServerStatus::Setup, SvcStatus::Running);
        assert_eq!(label, "Service: First-run setup pending");
    }

    #[test]
    fn compose_label_distinguishes_stopped_from_pending() {
        let stopped = compose_status_label(ServerStatus::Stopped, SvcStatus::Stopped);
        assert!(stopped.contains("Stopped"));
        let pending = compose_status_label(ServerStatus::Stopped, SvcStatus::Pending);
        assert!(pending.contains("starting"));
    }

    #[test]
    fn compose_label_surfaces_scm_error() {
        let label = compose_status_label(ServerStatus::Stopped, SvcStatus::Error);
        assert!(label.contains("SCM"));
    }
}
