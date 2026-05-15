//! Probe the local execlaw server's health. Used by the tray to
//! surface "Running / Setup / Stopped / Error" in real time.
//!
//! We hit `/api/ping` rather than `/api/health` because ping
//! distinguishes three first-run states the user cares about:
//!
//!   * `setup`  — no controller user yet (operator must finish the
//!                first-run wizard).
//!   * `wizard` — controller exists but no backend configured yet.
//!   * `pong`   — fully operational.
//!
//! Any non-200 / no-connect maps to `Stopped` (the LaunchAgent
//! probably hasn't come up yet) or `Error` (server is up but
//! malfunctioning).

use std::time::Duration;

/// User-facing state for the tray status row. The variants serialise
/// to short strings that fit in a menu bar label.
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

/// Single probe against the server. 1.5s budget — long enough for
/// a sluggish first boot when the agent is launching the inference
/// container, short enough that the tray feels responsive when the
/// server is just down.
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
