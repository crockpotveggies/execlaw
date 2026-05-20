//! Tauri app wiring — tray, menu, window, status polling, and the
//! handful of click handlers that map to systemd lifecycle calls.
//!
//! Lifetime model:
//!
//! - The .deb installs binaries only (the bundled `execlaw` server
//!   + this tray app + plugin ZIPs). The systemd user unit is
//!   registered LAZILY by this tray app on first launch — same
//!   pattern the macOS tray uses with `SMAppService.register()`.
//!   apt's postinst hooks would have to run as root and `--user`
//!   systemd units need to live in the operator's HOME, so the
//!   "register on tray launch" pattern is cleaner than passing
//!   `$SUDO_USER` through a postinst script.
//! - Quitting the tray DOES NOT stop the systemd unit. Use the
//!   *Restart service* / *Uninstall execlaw…* menu items, or
//!   `systemctl --user stop execlaw` from a shell.
//! - `apt remove execlaw` removes binaries but leaves the user's
//!   systemd unit in place (per-user units are out of apt's
//!   reach). To fully clean up: tray's *Uninstall execlaw…* first,
//!   then `apt remove`.
//!
//! This file mirrors `desktop-macos/src-tauri/src/app.rs` and
//! `desktop-windows/src-tauri/src/app.rs` — menu item IDs, status
//! row composition, and click semantics are deliberately identical
//! so all three platforms feel the same. Linux-specific
//! differences are localised to:
//!
//!   * Tray icon is a colored PNG (no template-image concept like
//!     macOS, no .ico like Windows).
//!   * No activation-policy / dock-icon flip — Linux WMs handle
//!     z-order via standard X11 / Wayland surfaces.
//!   * "Open data folder" reveals `~/.execlaw/` via `xdg-open`.
//!   * "Restart service" / "Uninstall…" don't need privilege
//!     escalation — user systemd units run under the operator's
//!     UID, no `pkexec` dance.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;

use crate::server_status::{self, ServerStatus};
use crate::systemd::{self, UnitStatus};

/// Loopback URL the SPA + API both live behind. Hardcoded to the
/// `SERVICE_BIND` constant the Rust CLI uses (`crates/cli/src/
/// service.rs::SERVICE_BIND`). If that constant ever changes, this
/// must change in lockstep.
const SERVER_URL: &str = "http://127.0.0.1:3031";

/// Menu item IDs — kept as constants so the click dispatcher can
/// pattern-match without typo risk. Names mirror the macOS + Windows
/// siblings so future cross-platform refactors don't need to
/// translate.
mod menu_ids {
    pub const STATUS: &str = "status";
    pub const OPEN: &str = "open";
    pub const VIEW_LOGS: &str = "view_logs";
    pub const RESTART: &str = "restart";
    pub const DATA_FOLDER: &str = "data_folder";
    pub const UNINSTALL: &str = "uninstall";
    pub const QUIT: &str = "quit";
}

/// Tray menu items we need to mutate after construction. `status`
/// is the always-visible label; `view_logs` is the affordance the
/// poller enables only when the service is failed / errored.
struct MenuHandles {
    status: MenuItem<tauri::Wry>,
    view_logs: MenuItem<tauri::Wry>,
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
            // Build menu shell + tray. The status row is added
            // first as a disabled item ("Service: …") so it acts
            // as a label.
            let status = MenuItem::with_id(
                app,
                menu_ids::STATUS,
                "Service: starting…",
                false,
                None::<&str>,
            )?;
            let view_logs = MenuItem::with_id(
                app,
                menu_ids::VIEW_LOGS,
                "View logs (journalctl)…",
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
                    &view_logs,
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

            // Hide the View logs row by default — the systemd
            // poller re-enables it only when the unit is failed /
            // errored. Tauri 2's MenuItem has no hide/show; we
            // toggle enabled state and rewrite the label so it's
            // clear it isn't actionable. Mirrors the macOS Approve
            // row + Windows services.msc row patterns.
            view_logs.set_enabled(false)?;
            view_logs.set_text("(service OK)")?;

            let state = TrayState {
                menu: Mutex::new(MenuHandles {
                    status: status.clone(),
                    view_logs: view_logs.clone(),
                }),
                http: reqwest::Client::builder()
                    .user_agent("execlaw-tray-linux/0.1")
                    .build()
                    .expect("reqwest client builds with default config"),
            };
            app.manage(Arc::new(state));

            // Tray icon — embed the colored 128px PNG at compile
            // time so it ships in the binary, no I/O at boot.
            // Linux SNI (StatusNotifierItem) takes a PNG; no
            // template-image concept like macOS uses, no .ico like
            // Windows. The icon shows the same colors on light and
            // dark panels alike.
            let tray_image =
                tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))
                    .unwrap_or_else(|_| tauri::image::Image::new_owned(vec![0u8; 4], 1, 1));
            let _tray = TrayIconBuilder::with_id("execlaw-tray")
                .menu(&menu)
                .menu_on_left_click(true)
                .icon(tray_image)
                .tooltip("execlaw")
                .on_menu_event(handle_menu_event)
                .on_tray_icon_event(|_tray, event| {
                    // Left-click already opens the menu via
                    // `menu_on_left_click(true)`. Double-click is a
                    // no-op on most Linux SNI implementations
                    // because the SNI spec doesn't include a
                    // double-click activation gesture — we register
                    // the handler for completeness.
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        // intentional no-op
                    }
                })
                .build(app)?;

            // Register the systemd user unit on every launch.
            // service-manager's systemd backend treats install on
            // top of an existing unit as a noop + write-through, so
            // this is safe to run unconditionally. Failures land in
            // the tracing layer; the tray status row will reflect
            // the eventual reality on the next poll regardless.
            // Mirrors the macOS tray's call to
            // `SMAppService.register()` and the Windows NSIS
            // installer's post-install hook.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let dir = systemd::current_exe_dir();
                if let Err(e) = systemd::register_user_service(&dir).await {
                    tracing::warn!(error = %e, "user-service register failed at launch");
                }
                // Best-effort start. If the service is already
                // running, the CLI's `service start --user` is a
                // noop; if it failed to install above, this fails
                // too but harmlessly.
                if let Err(e) = systemd::start_user_service(&dir).await {
                    tracing::warn!(error = %e, "user-service start failed at launch");
                }
                drop(app_handle);
            });

            // Spawn the status poller. Polls every 5 s — same
            // cadence as the macOS + Windows siblings so the user
            // experience matches across platforms.
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
            // app, the SNI icon is the only persistent UI. Linux
            // window managers don't have macOS's Accessory/Regular
            // distinction; closing the chat window just hides it.
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                api.prevent_exit();
            }
        });
}

/// Dispatch menu clicks to the right handler.
fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        menu_ids::OPEN => open_chat_window(app),
        menu_ids::VIEW_LOGS => spawn_journal_viewer(),
        menu_ids::RESTART => restart_service(app),
        menu_ids::DATA_FOLDER => open_data_folder(app),
        menu_ids::UNINSTALL => uninstall_flow(app),
        menu_ids::QUIT => {
            // Quitting the tray DOES NOT stop the systemd unit.
            // That's the contract — `systemctl --user is-active
            // execlaw` will still report `active` afterwards.
            app.exit(0);
        }
        // Status row is disabled, shouldn't fire — silent on any
        // unknown id.
        _ => {}
    }
}

/// Open (or focus) the chat UI window pointed at the local server.
///
/// Unlike macOS where we have to flip the activation policy from
/// Accessory to Regular to surface the window above other apps,
/// Linux WMs handle this through standard X11 / Wayland focus
/// semantics. We call `unminimize()` + `set_focus()` so a
/// previously-minimised chat window comes back to the foreground
/// when the operator clicks "Open execlaw" a second time.
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

/// Reveal `~/.execlaw/` in the file manager. Creates the dir first
/// if it doesn't exist so the open call always lands somewhere
/// reasonable on a fresh install (the server creates it on first
/// run, but the operator might click this before the systemd unit
/// has fully booted).
fn open_data_folder(app: &AppHandle) {
    let Some(home) = home_dir() else {
        tracing::warn!("could not resolve $HOME for data folder reveal");
        return;
    };
    let data_dir = home.join(".execlaw");
    let _ = std::fs::create_dir_all(&data_dir);
    if let Err(e) = app
        .opener()
        .reveal_item_in_dir(data_dir.to_string_lossy().as_ref())
    {
        tracing::warn!(error = %e, "open data folder failed");
    }
}

/// Resolve the user's home directory. `$HOME` is set on every
/// systemd-user session; the tray inherits it.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Spawn a terminal running `journalctl --user -u execlaw -f`.
/// Used by the *View logs* menu item.
fn spawn_journal_viewer() {
    tauri::async_runtime::spawn(async {
        systemd::open_journal_viewer().await;
    });
}

/// Restart the systemd user unit. No privilege escalation needed —
/// user units run under the operator's UID and `systemctl --user`
/// is allowed without sudo.
fn restart_service(app: &AppHandle) {
    let app_for_dialog = app.clone();
    tauri::async_runtime::spawn(async move {
        let dir = systemd::current_exe_dir();
        if let Err(e) = systemd::restart_user_service(&dir).await {
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
/// On Linux, the .deb itself is removed via `apt remove execlaw` —
/// this menu item ONLY clears the per-user systemd registration and
/// optionally the data dir. The README spells out the two-step
/// teardown for operators who want a full wipe.
fn uninstall_flow(app: &AppHandle) {
    let app_for_async = app.clone();
    tauri::async_runtime::spawn(async move {
        let confirmed = app_for_async
            .dialog()
            .message(
                "This stops the execlaw background service and removes its systemd \
                 user-unit registration. Your data at ~/.execlaw/ is preserved unless \
                 you also confirm deletion in the next step.\n\n\
                 Note: this does NOT remove the installed program — use \
                 `sudo apt remove execlaw` (or your distro's equivalent) for a full \
                 removal.\n\n\
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
        let dir = systemd::current_exe_dir();
        if let Err(e) = systemd::uninstall_user_service(&dir).await {
            show_error(
                &app_for_async,
                "Uninstall failed",
                &format!("Could not deregister the execlaw service: {e}"),
            );
            return;
        }

        let delete_data = app_for_async
            .dialog()
            .message(
                "Also delete your data at ~/.execlaw/?\n\n\
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
            if let Some(home) = home_dir() {
                let data_dir = home.join(".execlaw");
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
                "execlaw service has been removed. To finish uninstalling the program, \
                 run `sudo apt remove execlaw` (or your distro's equivalent).",
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

/// Poll the server every 5 s and rewrite the status row + the
/// View-logs-fallback row based on the result + systemd status.
async fn run_status_poller(app: AppHandle) {
    let state = app.state::<Arc<TrayState>>().inner().clone();
    loop {
        let server = server_status::probe(&state.http, SERVER_URL).await;
        let unit = systemd::query().await;

        let label = compose_status_label(server, unit);
        let needs_logs = matches!(unit, UnitStatus::Failed | UnitStatus::Error);

        // Hold the lock only across the (synchronous) menu mutation
        // calls so we don't serialise the 5 s poll behind UI work.
        let menu = state.menu.lock().await;
        if let Err(e) = menu.status.set_text(&label) {
            tracing::warn!(error = %e, "status item set_text failed");
        }
        if needs_logs {
            let _ = menu.view_logs.set_enabled(true);
            let _ = menu.view_logs.set_text("View logs (journalctl)…");
        } else {
            let _ = menu.view_logs.set_enabled(false);
            let _ = menu.view_logs.set_text("(service OK)");
        }
        drop(menu);

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Compose the tray status row. Combines the server-side health
/// probe with the systemd unit state so the operator can
/// distinguish "the unit isn't installed" from "the unit is
/// installed but the server hasn't come up yet" from "the unit
/// failed".
fn compose_status_label(server: ServerStatus, unit: UnitStatus) -> String {
    match (unit, server) {
        (UnitStatus::NotInstalled, _) => "Service: not installed".into(),
        (UnitStatus::Error, _) => "Service: systemctl query failed".into(),
        (UnitStatus::Failed, _) => "Service: failed — view logs".into(),
        (UnitStatus::Pending, _) => "Service: starting…".into(),
        (UnitStatus::Stopped, _) => "Service: Stopped".into(),
        // systemd says active — defer to the HTTP probe for the
        // user-facing wording so first-run wizard states surface.
        (UnitStatus::Running, s) => format!("Service: {}", s.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_label_promotes_not_installed_over_server_state() {
        let label = compose_status_label(ServerStatus::Stopped, UnitStatus::NotInstalled);
        assert!(label.contains("not installed"));
    }

    #[test]
    fn compose_label_uses_server_state_when_service_running() {
        let label = compose_status_label(ServerStatus::Running, UnitStatus::Running);
        assert_eq!(label, "Service: Running");
        let label = compose_status_label(ServerStatus::Setup, UnitStatus::Running);
        assert_eq!(label, "Service: First-run setup pending");
    }

    #[test]
    fn compose_label_surfaces_failed_with_logs_hint() {
        let label = compose_status_label(ServerStatus::Stopped, UnitStatus::Failed);
        assert!(label.contains("failed"));
        assert!(label.contains("logs"));
    }

    #[test]
    fn compose_label_distinguishes_stopped_from_pending() {
        let stopped = compose_status_label(ServerStatus::Stopped, UnitStatus::Stopped);
        assert!(stopped.contains("Stopped"));
        let pending = compose_status_label(ServerStatus::Stopped, UnitStatus::Pending);
        assert!(pending.contains("starting"));
    }
}
