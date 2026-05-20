//! Probe the local execlaw server's health. Used by the tray to
//! surface "Running / Setup / Stopped / Error" in real time.
//!
//! Verbatim port of `desktop-macos/src-tauri/src/server_status.rs`
//! and `desktop-windows/src-tauri/src/server_status.rs` — keep all
//! three in lockstep; the semantics MUST stay identical so an
//! operator on any of the three platforms sees the same status
//! strings.

use std::time::Duration;

/// User-facing state for the tray status row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Running,
    Setup,
    Wizard,
    Stopped,
    Error,
}

impl ServerStatus {
    pub fn label(&self) -> &'static str {
        match self {
            ServerStatus::Running => "Running",
            ServerStatus::Setup => "First-run setup pending",
            ServerStatus::Wizard => "Setup wizard pending",
            ServerStatus::Stopped => "Stopped",
            ServerStatus::Error => "Error",
        }
    }
}

/// Single probe against the server. 1.5 s budget — long enough for
/// a sluggish first boot when systemd is launching any sidecar
/// containers, short enough that the tray feels responsive when
/// the server is just down.
pub async fn probe(client: &reqwest::Client, base_url: &str) -> ServerStatus {
    let url = format!("{base_url}/api/ping");
    let req = client
        .get(&url)
        .timeout(Duration::from_millis(1500))
        .send()
        .await;
    match req {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            match body.trim() {
                "pong" => ServerStatus::Running,
                "setup" => ServerStatus::Setup,
                "wizard" => ServerStatus::Wizard,
                _ => ServerStatus::Error,
            }
        }
        Ok(_) => ServerStatus::Error,
        Err(e) if e.is_connect() || e.is_timeout() => ServerStatus::Stopped,
        Err(_) => ServerStatus::Error,
    }
}
