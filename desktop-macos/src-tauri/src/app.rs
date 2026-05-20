//! Tauri app wiring — tray, menu, window, status polling, and the
//! handful of click handlers that map to SMAppService calls.
//!
//! Lifetime model (per the scope conversation):
//!
//! - The .app registers the bundled LaunchAgent via SMAppService on
//!   first launch. launchd runs the agent independently — quitting
//!   the tray DOES NOT stop the server. Survives reboot.
//! - Dragging the .app to Trash → macOS auto-disables the agent.
//! - The "Uninstall execlaw…" menu item exists for the rarer case
//!   where the user wants to deregister + delete data without
//!   removing the .app.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Duration;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{
    ActivationPolicy, AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;

use crate::server_status::{self, ServerStatus};
use crate::sm_app_service::{self, AgentStatus};

/// Plist filename that ships inside the bundle at
/// `Contents/Library/LaunchAgents/`. Must match the file name in
/// `desktop-macos/src-tauri/macos/LaunchAgents/`.
const AGENT_PLIST_NAME: &str = "com.execlaw.agent.plist";

/// Loopback URL the SPA + API both live behind. Hardcoded to the
/// `SERVICE_BIND` constant the Rust CLI uses (`crates/cli/src/
/// service.rs::SERVICE_BIND`). If that constant ever changes, this
/// must change in lockstep.
const SERVER_URL: &str = "http://127.0.0.1:3031";

/// Menu item IDs — kept as constants so the click dispatcher can
/// pattern-match without typo risk.
mod menu_ids {
    pub const STATUS: &str = "status";
    pub const OPEN: &str = "open";
    pub const APPROVE: &str = "approve";
    pub const RESTART: &str = "restart";
    pub const DATA_FOLDER: &str = "data_folder";
    pub const UNINSTALL: &str = "uninstall";
    pub const QUIT: &str = "quit";
}

/// Tray menu items we need to mutate after construction (status
/// label, and the "Approve in System Settings…" row which is
/// only visible when status == RequiresApproval).
struct MenuHandles {
    status: MenuItem<tauri::Wry>,
    approve: MenuItem<tauri::Wry>,
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
            // Accessory == no Dock icon, no menu bar focus
            // stealing. This is the LSUIElement equivalent for
            // apps that don't ship an Info.plist override.
            app.set_activation_policy(ActivationPolicy::Accessory);

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
            let approve = MenuItem::with_id(
                app,
                menu_ids::APPROVE,
                "Approve in System Settings…",
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
                    &approve,
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

            // Stash mutable menu handles so the poller can update
            // the status row text and toggle the Approve row's
            // visibility without rebuilding the whole menu.
            let state = TrayState {
                menu: Mutex::new(MenuHandles {
                    status: status.clone(),
                    approve: approve.clone(),
                }),
                http: reqwest::Client::builder()
                    .user_agent("execlaw-tray/0.1")
                    .build()
                    .expect("reqwest client builds with default config"),
            };
            app.manage(Arc::new(state));

            // Hide the Approve row by default — the status poller
            // re-enables it only when the agent reports
            // RequiresApproval. (Tauri 2's MenuItem has no
            // hide/show; we toggle enabled state and rewrite the
            // label so it's clear it isn't actionable.)
            approve.set_enabled(false)?;
            approve.set_text("(no action required)")?;

            // Menu bar icon — embed the 44px (@2x) template PNG at
            // build time so it ships in the binary, no I/O at boot.
            // `icon_as_template(true)` tells macOS this is a template
            // image (black + alpha) and to recolor it to the system
            // text color so it adapts to light/dark menu bars. The
            // source SVG is monochrome (#1a1a1a on transparent) so
            // sips' PNG export is already a valid template.
            let tray_image =
                tauri::image::Image::from_bytes(include_bytes!("../icons/tray@2x.png"))
                    .unwrap_or_else(|_| tauri::image::Image::new_owned(vec![0u8; 4], 1, 1));
            let _tray = TrayIconBuilder::with_id("execlaw-tray")
                .menu(&menu)
                .menu_on_left_click(true)
                .icon(tray_image)
                .icon_as_template(true)
                .on_menu_event(handle_menu_event)
                .on_tray_icon_event(|_tray, event| {
                    // Left-click already opens the menu via
                    // `menu_on_left_click(true)`; nothing to do
                    // for right-click on macOS.
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        // Double-click opens the chat UI as a
                        // shortcut for the most common action.
                        // No handle here — dispatched via a
                        // posted MenuEvent in a future revision.
                    }
                })
                .build(app)?;

            // Register the agent on every launch. SMAppService
            // treats a re-register as an idempotent status
            // refresh, so this is safe to run unconditionally.
            // Failures land in the tracing layer; the tray status
            // row will reflect the eventual reality on the next
            // poll regardless.
            if let Err(e) = sm_app_service::register_agent(AGENT_PLIST_NAME) {
                tracing::warn!(error = %e, "agent register failed at launch");
            }

            // Spawn the status poller. Polls every 5s — fast
            // enough to feel live, slow enough to be invisible in
            // Activity Monitor. The first probe also gates the
            // initial label rewrite.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                run_status_poller(app_handle).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            match &event {
                // Don't quit when the last window closes — we're a
                // menu bar app, the tray is the only persistent UI.
                tauri::RunEvent::ExitRequested { api, .. } => {
                    api.prevent_exit();
                }
                // Window-lifecycle policy flip: when the chat window
                // is destroyed (operator closed it), drop the app
                // back to Accessory so the Dock icon disappears and
                // ⌘-Tab stops showing us. The reverse transition
                // (Accessory → Regular) happens in open_chat_window
                // when the window is first created or re-shown.
                //
                // We listen for `Destroyed` specifically — Tauri
                // destroys the window object on close-request unless
                // a handler intercepts. CloseRequested fires first
                // but the window is still alive at that point;
                // flipping the policy then leaves a brief flash of a
                // dockless half-open window. Destroyed is the clean
                // signal that the window is fully gone.
                tauri::RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::Destroyed,
                    ..
                } if label == "chat" => {
                    if let Err(e) =
                        app.set_activation_policy(ActivationPolicy::Accessory)
                    {
                        tracing::warn!(
                            error = %e,
                            "post-close set_activation_policy(Accessory) failed"
                        );
                    }
                }
                _ => {}
            }
        });
}

/// Dispatch menu clicks to the right handler.
fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        menu_ids::OPEN => open_chat_window(app),
        menu_ids::APPROVE => sm_app_service::open_login_items_settings(),
        menu_ids::RESTART => restart_agent(app),
        menu_ids::DATA_FOLDER => open_data_folder(app),
        menu_ids::UNINSTALL => uninstall_flow(app),
        menu_ids::QUIT => {
            // Quitting the tray DOES NOT stop the LaunchAgent.
            // That's the contract — `launchctl list` will still
            // show com.execlaw.agent enabled afterwards.
            app.exit(0);
        }
        // Status row is disabled, shouldn't fire — silent on
        // any unknown id.
        _ => {}
    }
}

/// Open (or focus) the chat UI window pointed at the local server.
///
/// macOS quirk: with `ActivationPolicy::Accessory` the app is
/// excluded from Dock + ⌘-Tab cycle, and Tauri's
/// `window.set_focus()` alone won't elevate the window above other
/// apps' windows when execlaw isn't the frontmost app. The
/// operator clicks "Open execlaw" expecting the window to surface,
/// not just gain "focus" behind whatever Slack tab they were
/// looking at. We call `-[NSApplication activateIgnoringOtherApps:]`
/// explicitly so the WindowServer hoists our window even though we
/// have no Dock presence.
fn open_chat_window(app: &AppHandle) {
    // Flip to Regular BEFORE showing so the WindowServer assigns the
    // chat window a Dock-managed slot from the start. If we stayed
    // Accessory the window would appear but the Dock icon would
    // never light up, ⌘-Tab would skip us, and Mission Control
    // wouldn't list the window as one of the operator's open apps.
    // The RunEvent::WindowEvent::Destroyed handler flips back to
    // Accessory once the operator closes the window.
    if let Err(e) = app.set_activation_policy(ActivationPolicy::Regular) {
        tracing::warn!(
            error = %e,
            "set_activation_policy(Regular) failed; window may not surface in Dock"
        );
    }

    if let Some(window) = app.get_webview_window("chat") {
        let _ = window.show();
        activate_app_ignoring_others();
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
            // First-time create — activate AFTER the WKWebView's
            // NSWindow is in the window list. Tauri returns from
            // build() once the window has been created on the main
            // thread, so this ordering is safe.
            activate_app_ignoring_others();
            let _ = window.set_focus();
        }
        Err(e) => tracing::error!(error = %e, "failed to open chat window"),
    }
}

/// Equivalent to `NSApp.activate(ignoringOtherApps: true)`. Required
/// for accessory apps to bring their windows above whatever the
/// operator was previously looking at.
fn activate_app_ignoring_others() {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;
    let Some(cls) = AnyClass::get("NSApplication") else {
        tracing::warn!("NSApplication class not available — activate skipped");
        return;
    };
    // SAFETY: `+[NSApplication sharedApplication]` returns the
    // singleton; `-[NSApplication activateIgnoringOtherApps:]` is a
    // void method on the main thread. Tauri menu-event handlers run
    // on the main thread, so this call is well-typed.
    unsafe {
        let app: *mut objc2::runtime::AnyObject = msg_send![cls, sharedApplication];
        if app.is_null() {
            tracing::warn!("NSApp sharedApplication returned nil — activate skipped");
            return;
        }
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
    }
}

/// Reveal `~/.execlaw/` in Finder. Creates the dir first if it
/// doesn't exist so the open call always lands somewhere
/// reasonable on a fresh install (the server creates it on first
/// run, but the operator might click this before the agent has
/// fully booted).
fn open_data_folder(app: &AppHandle) {
    let Some(home) = dirs_home() else {
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

/// Resolve the user's home directory without pulling the full
/// `dirs` crate just for this. `$HOME` is set on every macOS user
/// session; the launchd-spawned tray inherits it.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Restart by unregister + register. SMAppService doesn't expose
/// a direct restart verb, but launchd handles KeepAlive so the
/// agent comes back automatically after re-registration.
fn restart_agent(app: &AppHandle) {
    let app_for_dialog = app.clone();
    tauri::async_runtime::spawn(async move {
        let unreg = sm_app_service::unregister_agent(AGENT_PLIST_NAME);
        if let Err(e) = unreg {
            show_error(&app_for_dialog, "Restart failed", &format!("Unregister step: {e}"));
            return;
        }
        // Brief pause so launchd settles the unregister before
        // we re-register against the same label.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Err(e) = sm_app_service::register_agent(AGENT_PLIST_NAME) {
            show_error(&app_for_dialog, "Restart failed", &format!("Re-register step: {e}"));
        }
    });
}

/// Uninstall flow. Two confirmation gates:
///   1. "Stop and remove the execlaw service?"
///   2. "Also delete your data?" — the destructive one.
fn uninstall_flow(app: &AppHandle) {
    let app_for_async = app.clone();
    tauri::async_runtime::spawn(async move {
        let confirmed = app_for_async
            .dialog()
            .message(
                "This stops the execlaw background service and removes its LaunchAgent \
                 registration. Your data at ~/.execlaw/ is preserved unless you also \
                 confirm deletion in the next step.\n\nProceed?",
            )
            .title("Uninstall execlaw")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Uninstall".to_string(),
                "Cancel".to_string(),
            ))
            .blocking_show();
        if !confirmed {
            return;
        }
        if let Err(e) = sm_app_service::unregister_agent(AGENT_PLIST_NAME) {
            show_error(
                &app_for_async,
                "Uninstall failed",
                &format!("Could not unregister the LaunchAgent: {e}"),
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
            if let Some(home) = dirs_home() {
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
                "execlaw service has been removed. To finish uninstalling, drag \
                 execlaw.app from /Applications to the Trash.",
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
/// Approve-in-Settings row based on the result + agent status.
async fn run_status_poller(app: AppHandle) {
    let state = app
        .state::<Arc<TrayState>>()
        .inner()
        .clone();
    loop {
        let server = server_status::probe(&state.http, SERVER_URL).await;
        let agent = sm_app_service::agent_status(AGENT_PLIST_NAME)
            .unwrap_or(AgentStatus::Unknown(-1));

        let label = compose_status_label(server, agent);

        // Hold the lock only across the (synchronous) menu mutation
        // calls so we don't serialise the 5s poll behind UI work.
        let menu = state.menu.lock().await;
        if let Err(e) = menu.status.set_text(&label) {
            tracing::warn!(error = %e, "status item set_text failed");
        }
        let show_approve = matches!(agent, AgentStatus::RequiresApproval);
        if show_approve {
            let _ = menu.approve.set_enabled(true);
            let _ = menu.approve.set_text("Approve in System Settings…");
        } else {
            let _ = menu.approve.set_enabled(false);
            let _ = menu.approve.set_text("(no action required)");
        }
        drop(menu);

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Compose the tray status row. Combines the server-side health
/// probe with the SMAppService registry status, so the user can
/// distinguish "the agent isn't registered" from "the agent is
/// registered but the server hasn't come up yet."
fn compose_status_label(server: ServerStatus, agent: AgentStatus) -> String {
    match (agent, server) {
        (AgentStatus::RequiresApproval, _) => {
            "Service: needs approval — open Login Items".into()
        }
        (AgentStatus::NotRegistered, _) => "Service: not registered".into(),
        (AgentStatus::NotFound, _) => {
            "Service: plist missing (rebuild required)".into()
        }
        (AgentStatus::Unknown(n), _) => format!("Service: unknown agent state ({n})"),
        (AgentStatus::Enabled, s) => format!("Service: {}", s.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_label_promotes_approval_over_server_state() {
        // Even if the server is somehow up, the user-actionable
        // condition is the missing approval — surface that.
        let label = compose_status_label(ServerStatus::Running, AgentStatus::RequiresApproval);
        assert!(label.contains("approval"));
    }

    #[test]
    fn compose_label_includes_server_state_when_agent_enabled() {
        let label = compose_status_label(ServerStatus::Running, AgentStatus::Enabled);
        assert_eq!(label, "Service: Running");
        let label = compose_status_label(ServerStatus::Stopped, AgentStatus::Enabled);
        assert_eq!(label, "Service: Stopped");
    }

    #[test]
    fn compose_label_surfaces_missing_plist() {
        let label = compose_status_label(ServerStatus::Stopped, AgentStatus::NotFound);
        assert!(label.contains("plist missing"));
    }
}
