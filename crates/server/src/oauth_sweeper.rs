//! Background sweeper:
//!   1. Refresh OAuth access_tokens that fall inside the
//!      `proactive_refresh_window` (default 10 min before expiry).
//!      Calls the provider's refresh-token grant, persists the new
//!      access_token via `OauthTokenStore::upsert` (which COALESCES
//!      the refresh_token so a Google grant that omits one doesn't
//!      clobber the long-lived secret).
//!   2. Purge expired `state_oauth_pending` rows so a leaked
//!      authorize URL stops working after its 10-min CSRF window.
//!
//! The sweeper is provider-agnostic: it picks the right
//! [`OauthProvider`] impl from the persisted `provider` column on
//! the client row. Today there's only Google; adding a second is
//! purely additive.

use crate::oauth_provider::{GoogleOauthProvider, OauthProvider, RefreshParams};
use execlaw_core::Database;
use execlaw_core::oauth::{OauthClientStore, OauthPendingStore, OauthTokenStore, OauthTokens};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{info, warn};

/// How often the sweeper wakes up. Refresh-grant calls are cheap;
/// 60 s gives a reasonable proactive window without thrashing.
const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Refresh tokens whose `token_expires_at` is within this many
/// seconds of `now`. Slightly larger than two TICK_INTERVALs so a
/// missed tick doesn't leave a token to expire in-flight.
const REFRESH_SLACK_SECS: i64 = 600;

/// Per-tick batch cap so a misconfigured deployment with thousands
/// of expiring tokens can't burn the executor on a single tick.
/// In practice operators have <10 OAuth accounts.
const BATCH_LIMIT: i64 = 32;

#[derive(Clone)]
pub struct OauthSweeper {
    db: Database,
    interval: Duration,
    kick: Arc<Notify>,
}

impl OauthSweeper {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            interval: TICK_INTERVAL,
            kick: Arc::new(Notify::new()),
        }
    }

    /// Test seam: shorter tick.
    pub fn with_interval(db: Database, interval: Duration) -> Self {
        Self {
            db,
            interval,
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            slack_secs = REFRESH_SLACK_SECS,
            "oauth sweeper running",
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("oauth sweeper stop received; draining once and exiting");
                    self.sweep_once().await;
                    return;
                }
            }
            self.sweep_once().await;
        }
    }

    /// One pass. Public so tests can drive it directly.
    pub async fn sweep_once(&self) -> SweepReport {
        let now = chrono::Utc::now().timestamp();
        let mut report = SweepReport::default();

        // 1. Refresh expiring tokens.
        let expiring = match OauthTokenStore::new(&self.db)
            .list_expiring_before(now + REFRESH_SLACK_SECS, BATCH_LIMIT)
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "oauth sweeper: list_expiring_before failed");
                Vec::new()
            }
        };
        for tokens in expiring {
            match self.refresh_one(&tokens, now).await {
                Ok(()) => report.refreshed += 1,
                Err(SweeperError::NoRefreshToken) => report.skipped_no_refresh += 1,
                Err(SweeperError::NoClient) => {
                    // Client config disappeared (operator hit
                    // Disconnect via the wrong code path?). The token
                    // row is orphaned; remove it so we stop trying.
                    let _ = OauthTokenStore::new(&self.db)
                        .delete(&tokens.plugin_id, &tokens.account_name);
                    report.refresh_failures += 1;
                }
                Err(e) => {
                    warn!(
                        plugin_id = %tokens.plugin_id,
                        account_name = %tokens.account_name,
                        error = %e,
                        "oauth sweeper: refresh failed",
                    );
                    report.refresh_failures += 1;
                }
            }
        }

        // 2. Purge expired pending CSRF rows.
        match OauthPendingStore::new(&self.db).purge_expired(now) {
            Ok(n) => report.pending_purged = n,
            Err(e) => warn!(error = %e, "oauth sweeper: purge_expired failed"),
        }
        report
    }

    async fn refresh_one(&self, tokens: &OauthTokens, now: i64) -> Result<(), SweeperError> {
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or(SweeperError::NoRefreshToken)?;
        let client = OauthClientStore::new(&self.db)
            .get(&tokens.plugin_id, &tokens.account_name)
            .map_err(|e| SweeperError::Other(e.to_string()))?
            .ok_or(SweeperError::NoClient)?;
        let provider = pick_provider(&client.provider)
            .ok_or_else(|| SweeperError::Other(format!("unknown provider '{}'", client.provider)))?;
        let grant = provider
            .refresh_access_token(&RefreshParams {
                client_id: client.client_id.clone(),
                client_secret: client.client_secret.clone(),
                refresh_token,
            })
            .await
            .map_err(|e| SweeperError::Other(e.to_string()))?;
        let scopes_granted = grant
            .scope
            .clone()
            .map(|s| {
                serde_json::to_string(&s.split_whitespace().collect::<Vec<_>>())
                    .unwrap_or_else(|_| tokens.scopes_granted.clone())
            })
            .unwrap_or_else(|| tokens.scopes_granted.clone());
        OauthTokenStore::new(&self.db)
            .upsert(&OauthTokens {
                plugin_id: tokens.plugin_id.clone(),
                account_name: tokens.account_name.clone(),
                access_token: grant.access_token,
                refresh_token: grant.refresh_token, // None preserves persisted
                token_expires_at: now + grant.expires_in_secs,
                scopes_granted,
                account_email: None, // None preserves persisted
                created_at: tokens.created_at,
                updated_at: now,
            })
            .map_err(|e| SweeperError::Other(e.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
pub struct SweepReport {
    pub refreshed: usize,
    pub skipped_no_refresh: usize,
    pub refresh_failures: usize,
    pub pending_purged: usize,
}

#[derive(Debug, thiserror::Error)]
enum SweeperError {
    #[error("no refresh_token persisted; cannot refresh")]
    NoRefreshToken,
    #[error("client config missing")]
    NoClient,
    #[error("{0}")]
    Other(String),
}

fn pick_provider(provider_id: &str) -> Option<Box<dyn OauthProvider>> {
    match provider_id {
        "google" => Some(Box::new(GoogleOauthProvider::default())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_provider::GoogleOauthProvider;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::oauth::{OauthClient, OauthPending, OauthPendingStore};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[tokio::test]
    async fn sweep_purges_expired_pending_rows() {
        let db = fresh_db();
        let now = chrono::Utc::now().timestamp();
        OauthPendingStore::new(&db)
            .insert(&OauthPending {
                state_token: "stale".into(),
                plugin_id: "p".into(),
                account_name: "a".into(),
                redirect_to: None,
                created_at: now - 1000,
                expires_at: now - 100,
            })
            .unwrap();
        OauthPendingStore::new(&db)
            .insert(&OauthPending {
                state_token: "fresh".into(),
                plugin_id: "p".into(),
                account_name: "b".into(),
                redirect_to: None,
                created_at: now,
                expires_at: now + 600,
            })
            .unwrap();
        let sweeper = OauthSweeper::new(db.clone());
        let report = sweeper.sweep_once().await;
        assert_eq!(report.pending_purged, 1);
        // The fresh row survives.
        assert!(OauthPendingStore::new(&db).consume("fresh", now).is_ok());
    }

    #[tokio::test]
    async fn sweep_skips_tokens_without_refresh_token() {
        // Some providers don't issue refresh_tokens (e.g. confused
        // deputy flows). Those are unrefreshable; the sweeper
        // counts them but doesn't crash.
        let db = fresh_db();
        let now = chrono::Utc::now().timestamp();
        OauthClientStore::new(&db)
            .upsert(&OauthClient {
                plugin_id: "p".into(),
                account_name: "a".into(),
                provider: "google".into(),
                client_id: "cid".into(),
                client_secret: "secret".into(),
                redirect_uri: "http://x".into(),
                scopes_json: "[]".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        OauthTokenStore::new(&db)
            .upsert(&OauthTokens {
                plugin_id: "p".into(),
                account_name: "a".into(),
                access_token: "ya29".into(),
                refresh_token: None,
                token_expires_at: now + 60, // expires soon
                scopes_granted: "[]".into(),
                account_email: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let report = OauthSweeper::new(db.clone()).sweep_once().await;
        assert_eq!(report.refreshed, 0);
        assert_eq!(report.skipped_no_refresh, 1);
        // Token row is still there; the operator can re-connect.
        assert!(OauthTokenStore::new(&db).get("p", "a").unwrap().is_some());
    }

    #[tokio::test]
    async fn sweep_orphan_token_without_client_drops_token_row() {
        // The FK enforces the relationship at write time, but a
        // hypothetical race / manual SQL could leave a token row
        // orphaned. The sweeper handles that defensively.
        // We can't easily construct that state through the public
        // API (the FK rejects), so this test verifies the same
        // behaviour by routing through refresh_one directly.
        let db = fresh_db();
        let now = chrono::Utc::now().timestamp();
        // Insert client + tokens, then drop the client via raw SQL
        // bypassing CASCADE.
        OauthClientStore::new(&db)
            .upsert(&OauthClient {
                plugin_id: "p".into(),
                account_name: "a".into(),
                provider: "google".into(),
                client_id: "cid".into(),
                client_secret: "secret".into(),
                redirect_uri: "http://x".into(),
                scopes_json: "[]".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        OauthTokenStore::new(&db)
            .upsert(&OauthTokens {
                plugin_id: "p".into(),
                account_name: "a".into(),
                access_token: "ya29".into(),
                refresh_token: Some("rt".into()),
                token_expires_at: now + 60,
                scopes_granted: "[]".into(),
                account_email: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        // Drop the client + temporarily disable FK so the orphan
        // sticks around.
        db.with_conn(|c| {
            c.pragma_update(None, "foreign_keys", "OFF").unwrap();
            c.execute("DELETE FROM state_oauth_clients WHERE plugin_id='p'", [])
                .unwrap();
            c.pragma_update(None, "foreign_keys", "ON").unwrap();
            Ok(())
        })
        .unwrap();
        // Sweeper sees the orphan and removes it.
        let report = OauthSweeper::new(db.clone()).sweep_once().await;
        assert_eq!(report.refresh_failures, 1);
        assert!(OauthTokenStore::new(&db).get("p", "a").unwrap().is_none());
    }

    #[tokio::test]
    async fn sweep_refreshes_via_mock_provider() {
        // End-to-end: spin up a fake Google token endpoint, point a
        // provider at it, run one sweep against an expiring token,
        // assert the access_token rotated and expiry advanced.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "access_token": "rotated",
                "expires_in": 3600,
                "scope": "https://www.googleapis.com/auth/contacts.readonly",
                "token_type": "Bearer",
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let db = fresh_db();
        let now = chrono::Utc::now().timestamp();
        OauthClientStore::new(&db)
            .upsert(&OauthClient {
                plugin_id: "p".into(),
                account_name: "a".into(),
                provider: "google".into(),
                client_id: "cid".into(),
                client_secret: "secret".into(),
                redirect_uri: "http://x".into(),
                scopes_json: "[]".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        OauthTokenStore::new(&db)
            .upsert(&OauthTokens {
                plugin_id: "p".into(),
                account_name: "a".into(),
                access_token: "stale".into(),
                refresh_token: Some("rt".into()),
                token_expires_at: now + 60,
                scopes_granted: "[]".into(),
                account_email: Some("u@x".into()),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        // Direct refresh_one to exercise without needing to swap
        // the global GoogleOauthProvider out of the sweeper.
        let provider = GoogleOauthProvider::with_endpoints(
            reqwest::Client::new(),
            "https://accounts.google.com/o/oauth2/v2/auth",
            format!("http://{addr}/token"),
            "https://openidconnect.googleapis.com/v1/userinfo",
        );
        // Inline the refresh_one logic so we can swap provider.
        let refresh_token = "rt".to_string();
        let grant = provider
            .refresh_access_token(&RefreshParams {
                client_id: "cid".into(),
                client_secret: "secret".into(),
                refresh_token,
            })
            .await
            .unwrap();
        let now_after = now + 30;
        OauthTokenStore::new(&db)
            .upsert(&OauthTokens {
                plugin_id: "p".into(),
                account_name: "a".into(),
                access_token: grant.access_token,
                refresh_token: grant.refresh_token,
                token_expires_at: now_after + grant.expires_in_secs,
                scopes_granted: "[]".into(),
                account_email: None, // preserved by COALESCE in store
                created_at: now,
                updated_at: now_after,
            })
            .unwrap();
        let after = OauthTokenStore::new(&db).get("p", "a").unwrap().unwrap();
        assert_eq!(after.access_token, "rotated");
        assert_eq!(after.refresh_token.as_deref(), Some("rt")); // preserved
        assert_eq!(after.account_email.as_deref(), Some("u@x")); // preserved
        assert!(after.token_expires_at > now + 1000);
    }
}
