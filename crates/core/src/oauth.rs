//! OAuth 2.0 client + token persistence for plugin-declared
//! `[[oauth_accounts]]` entries.
//!
//! Three stores under one module so a future second OAuth-using
//! plugin (google-calendar, microsoft-365, ...) doesn't have to
//! re-derive the storage shape:
//!
//! * [`OauthClientStore`]   — operator-supplied client credentials
//!   (client_id / client_secret / redirect_uri) per (plugin_id,
//!   account_name). Lifecycle: written from Settings → <plugin> page.
//! * [`OauthTokenStore`]    — access_token / refresh_token / expiry,
//!   written by the callback handler, refreshed by the sweeper.
//! * [`OauthPendingStore`]  — short-lived CSRF state during the
//!   authorize redirect, consumed by the callback handler.
//!
//! Sensitive blobs (`client_secret`, `refresh_token`, `access_token`)
//! live in regular columns; SQLCipher encrypts the DB at rest, so
//! these never hit disk in plaintext. Vault-row indirection isn't
//! needed for this layer.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OauthError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("oauth client not configured for plugin '{plugin_id}' account '{account_name}'")]
    NoClient {
        plugin_id: String,
        account_name: String,
    },
    #[error("oauth tokens not present for plugin '{plugin_id}' account '{account_name}'")]
    NoTokens {
        plugin_id: String,
        account_name: String,
    },
    #[error("oauth state token not found or expired")]
    NoPending,
}

/// Operator-supplied OAuth client credentials. The plugin's
/// manifest declares the provider + scopes; the operator supplies
/// the secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthClient {
    pub plugin_id: String,
    pub account_name: String,
    pub provider: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    /// JSON-encoded `Vec<String>`. The Settings page persists the
    /// manifest-declared scopes here so the authorize URL can be
    /// constructed without re-reading the manifest.
    pub scopes_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl OauthClient {
    pub fn scopes(&self) -> Vec<String> {
        serde_json::from_str(&self.scopes_json).unwrap_or_default()
    }
}

/// Tokens for one connected account. `access_token` is the
/// short-lived bearer the plugin actually uses; `refresh_token` is
/// the long-lived secret the sweeper exchanges for fresh access
/// tokens before they expire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthTokens {
    pub plugin_id: String,
    pub account_name: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub token_expires_at: i64,
    /// JSON-encoded `Vec<String>` — what the user actually granted
    /// (may be a subset of what we asked for).
    pub scopes_granted: String,
    pub account_email: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl OauthTokens {
    pub fn scopes(&self) -> Vec<String> {
        serde_json::from_str(&self.scopes_granted).unwrap_or_default()
    }

    /// `true` when the access token has fewer than `slack_secs`
    /// seconds left before expiry. Sweeper passes a positive slack
    /// (e.g. 600s) so it can refresh ahead of time; per-call
    /// callers pass 0 (or a small grace) to gate "should I refresh
    /// before this request?"
    pub fn is_expiring(&self, now: i64, slack_secs: i64) -> bool {
        self.token_expires_at - slack_secs <= now
    }
}

/// Short-lived CSRF state used to bind the authorize redirect to
/// the eventual callback. Looked up by random `state_token` blob;
/// expired entries get GC'd by the sweeper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthPending {
    pub state_token: String,
    pub plugin_id: String,
    pub account_name: String,
    pub redirect_to: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

// ---------------------------------------------------------------------------
// Stores

pub struct OauthClientStore<'db> {
    db: &'db Database,
}

impl<'db> OauthClientStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Upsert (operator just hit Save in the Settings page). `now` is
    /// the unix-seconds timestamp the row's `updated_at` carries.
    pub fn upsert(&self, c: &OauthClient) -> Result<(), OauthError> {
        let plugin_id = c.plugin_id.clone();
        let account_name = c.account_name.clone();
        let provider = c.provider.clone();
        let client_id = c.client_id.clone();
        let client_secret = c.client_secret.clone();
        let redirect_uri = c.redirect_uri.clone();
        let scopes_json = c.scopes_json.clone();
        let created_at = c.created_at;
        let updated_at = c.updated_at;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO state_oauth_clients(plugin_id, account_name, provider, client_id, client_secret, redirect_uri, scopes_json, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(plugin_id, account_name) DO UPDATE SET \
                    provider = excluded.provider, \
                    client_id = excluded.client_id, \
                    client_secret = excluded.client_secret, \
                    redirect_uri = excluded.redirect_uri, \
                    scopes_json = excluded.scopes_json, \
                    updated_at = excluded.updated_at",
                params![
                    plugin_id,
                    account_name,
                    provider,
                    client_id,
                    client_secret,
                    redirect_uri,
                    scopes_json,
                    created_at,
                    updated_at,
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn get(
        &self,
        plugin_id: &str,
        account_name: &str,
    ) -> Result<Option<OauthClient>, OauthError> {
        let pid = plugin_id.to_owned();
        let acc = account_name.to_owned();
        let row = self.db.with_conn(|conn| {
            let r = conn
                .query_row(
                    "SELECT plugin_id, account_name, provider, client_id, client_secret, redirect_uri, scopes_json, created_at, updated_at \
                     FROM state_oauth_clients WHERE plugin_id = ?1 AND account_name = ?2",
                    params![pid, acc],
                    |row| {
                        Ok(OauthClient {
                            plugin_id: row.get(0)?,
                            account_name: row.get(1)?,
                            provider: row.get(2)?,
                            client_id: row.get(3)?,
                            client_secret: row.get(4)?,
                            redirect_uri: row.get(5)?,
                            scopes_json: row.get(6)?,
                            created_at: row.get(7)?,
                            updated_at: row.get(8)?,
                        })
                    },
                )
                .ok();
            Ok(r)
        })?;
        Ok(row)
    }

    /// Delete the client config and (via FK CASCADE) any tokens.
    /// Returns true when a row was actually removed.
    pub fn delete(&self, plugin_id: &str, account_name: &str) -> Result<bool, OauthError> {
        let pid = plugin_id.to_owned();
        let acc = account_name.to_owned();
        let n = self.db.with_conn(|conn| {
            let n = conn.execute(
                "DELETE FROM state_oauth_clients WHERE plugin_id = ?1 AND account_name = ?2",
                params![pid, acc],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

pub struct OauthTokenStore<'db> {
    db: &'db Database,
}

impl<'db> OauthTokenStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn upsert(&self, t: &OauthTokens) -> Result<(), OauthError> {
        let plugin_id = t.plugin_id.clone();
        let account_name = t.account_name.clone();
        let access_token = t.access_token.clone();
        let refresh_token = t.refresh_token.clone();
        let token_expires_at = t.token_expires_at;
        let scopes_granted = t.scopes_granted.clone();
        let account_email = t.account_email.clone();
        let created_at = t.created_at;
        let updated_at = t.updated_at;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO state_oauth_tokens(plugin_id, account_name, access_token, refresh_token, token_expires_at, scopes_granted, account_email, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(plugin_id, account_name) DO UPDATE SET \
                    access_token = excluded.access_token, \
                    refresh_token = COALESCE(excluded.refresh_token, state_oauth_tokens.refresh_token), \
                    token_expires_at = excluded.token_expires_at, \
                    scopes_granted = excluded.scopes_granted, \
                    account_email = COALESCE(excluded.account_email, state_oauth_tokens.account_email), \
                    updated_at = excluded.updated_at",
                params![
                    plugin_id,
                    account_name,
                    access_token,
                    refresh_token,
                    token_expires_at,
                    scopes_granted,
                    account_email,
                    created_at,
                    updated_at,
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn get(
        &self,
        plugin_id: &str,
        account_name: &str,
    ) -> Result<Option<OauthTokens>, OauthError> {
        let pid = plugin_id.to_owned();
        let acc = account_name.to_owned();
        let row = self.db.with_conn(|conn| {
            let r = conn
                .query_row(
                    "SELECT plugin_id, account_name, access_token, refresh_token, token_expires_at, scopes_granted, account_email, created_at, updated_at \
                     FROM state_oauth_tokens WHERE plugin_id = ?1 AND account_name = ?2",
                    params![pid, acc],
                    |row| {
                        Ok(OauthTokens {
                            plugin_id: row.get(0)?,
                            account_name: row.get(1)?,
                            access_token: row.get(2)?,
                            refresh_token: row.get(3)?,
                            token_expires_at: row.get(4)?,
                            scopes_granted: row.get(5)?,
                            account_email: row.get(6)?,
                            created_at: row.get(7)?,
                            updated_at: row.get(8)?,
                        })
                    },
                )
                .ok();
            Ok(r)
        })?;
        Ok(row)
    }

    /// All token rows expiring before `before`. Used by the refresh
    /// sweeper. Ordered by `token_expires_at` ASC so the earliest
    /// expiries refresh first.
    pub fn list_expiring_before(
        &self,
        before: i64,
        limit: i64,
    ) -> Result<Vec<OauthTokens>, OauthError> {
        let rows = self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT plugin_id, account_name, access_token, refresh_token, token_expires_at, scopes_granted, account_email, created_at, updated_at \
                 FROM state_oauth_tokens \
                 WHERE token_expires_at <= ?1 \
                 ORDER BY token_expires_at ASC \
                 LIMIT ?2",
            )?;
            let rows: Vec<OauthTokens> = stmt
                .query_map(params![before, limit], |row| {
                    Ok(OauthTokens {
                        plugin_id: row.get(0)?,
                        account_name: row.get(1)?,
                        access_token: row.get(2)?,
                        refresh_token: row.get(3)?,
                        token_expires_at: row.get(4)?,
                        scopes_granted: row.get(5)?,
                        account_email: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                })?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Drop tokens (operator clicked Disconnect, or refresh
    /// permanently failed). Returns true when a row was removed.
    pub fn delete(&self, plugin_id: &str, account_name: &str) -> Result<bool, OauthError> {
        let pid = plugin_id.to_owned();
        let acc = account_name.to_owned();
        let n = self.db.with_conn(|conn| {
            let n = conn.execute(
                "DELETE FROM state_oauth_tokens WHERE plugin_id = ?1 AND account_name = ?2",
                params![pid, acc],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

pub struct OauthPendingStore<'db> {
    db: &'db Database,
}

impl<'db> OauthPendingStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn insert(&self, p: &OauthPending) -> Result<(), OauthError> {
        let st = p.state_token.clone();
        let pid = p.plugin_id.clone();
        let acc = p.account_name.clone();
        let red = p.redirect_to.clone();
        let created = p.created_at;
        let expires = p.expires_at;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO state_oauth_pending(state_token, plugin_id, account_name, redirect_to, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![st, pid, acc, red, created, expires],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Atomically look up + delete the row. Single-use semantics so
    /// a leaked authorize URL can't be replayed. Returns
    /// `Err(NoPending)` if nothing matches OR the row had expired
    /// (the lookup applies the expires_at predicate).
    pub fn consume(&self, state_token: &str, now: i64) -> Result<OauthPending, OauthError> {
        let st = state_token.to_owned();
        let row = self.db.transaction(|tx| {
            let r: Option<OauthPending> = tx
                .query_row(
                    "SELECT state_token, plugin_id, account_name, redirect_to, created_at, expires_at \
                     FROM state_oauth_pending WHERE state_token = ?1 AND expires_at > ?2",
                    params![st, now],
                    |row| {
                        Ok(OauthPending {
                            state_token: row.get(0)?,
                            plugin_id: row.get(1)?,
                            account_name: row.get(2)?,
                            redirect_to: row.get(3)?,
                            created_at: row.get(4)?,
                            expires_at: row.get(5)?,
                        })
                    },
                )
                .ok();
            if r.is_some() {
                tx.execute(
                    "DELETE FROM state_oauth_pending WHERE state_token = ?1",
                    params![st],
                )?;
            }
            Ok(r)
        })?;
        row.ok_or(OauthError::NoPending)
    }

    /// Number of rows removed.
    pub fn purge_expired(&self, now: i64) -> Result<usize, OauthError> {
        let n = self.db.with_conn(|conn| {
            let n = conn.execute(
                "DELETE FROM state_oauth_pending WHERE expires_at <= ?1",
                params![now],
            )?;
            Ok(n)
        })?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn sample_client() -> OauthClient {
        OauthClient {
            plugin_id: "plugin-google-contacts".into(),
            account_name: "controller".into(),
            provider: "google".into(),
            client_id: "abc.apps.googleusercontent.com".into(),
            client_secret: "GOCSPX-secret".into(),
            redirect_uri: "http://localhost:3030/api/oauth/google/callback".into(),
            scopes_json: serde_json::to_string(&vec![
                "https://www.googleapis.com/auth/contacts.readonly".to_owned(),
            ])
            .unwrap(),
            created_at: 100,
            updated_at: 100,
        }
    }

    #[test]
    fn client_upsert_then_get_round_trips() {
        let db = fresh_db();
        let s = OauthClientStore::new(&db);
        let c = sample_client();
        s.upsert(&c).unwrap();
        let got = s.get(&c.plugin_id, &c.account_name).unwrap().unwrap();
        assert_eq!(got.client_id, c.client_id);
        assert_eq!(got.client_secret, c.client_secret);
        assert_eq!(
            got.scopes(),
            vec!["https://www.googleapis.com/auth/contacts.readonly"]
        );
    }

    #[test]
    fn client_upsert_overwrites_secret_and_bumps_updated_at() {
        let db = fresh_db();
        let s = OauthClientStore::new(&db);
        let mut c = sample_client();
        s.upsert(&c).unwrap();
        c.client_secret = "GOCSPX-rotated".into();
        c.updated_at = 200;
        s.upsert(&c).unwrap();
        let got = s.get(&c.plugin_id, &c.account_name).unwrap().unwrap();
        assert_eq!(got.client_secret, "GOCSPX-rotated");
        assert_eq!(got.updated_at, 200);
        assert_eq!(got.created_at, 100);
    }

    #[test]
    fn client_delete_cascades_to_tokens() {
        let db = fresh_db();
        let cs = OauthClientStore::new(&db);
        let ts = OauthTokenStore::new(&db);
        let c = sample_client();
        cs.upsert(&c).unwrap();
        ts.upsert(&OauthTokens {
            plugin_id: c.plugin_id.clone(),
            account_name: c.account_name.clone(),
            access_token: "ya29.access".into(),
            refresh_token: Some("1//refresh".into()),
            token_expires_at: 200,
            scopes_granted: serde_json::to_string(&vec!["x".to_owned()]).unwrap(),
            account_email: Some("user@example.com".into()),
            created_at: 100,
            updated_at: 100,
        })
        .unwrap();
        assert!(ts.get(&c.plugin_id, &c.account_name).unwrap().is_some());
        // Deleting the client removes the tokens via FK CASCADE.
        let removed = cs.delete(&c.plugin_id, &c.account_name).unwrap();
        assert!(removed);
        assert!(ts.get(&c.plugin_id, &c.account_name).unwrap().is_none());
    }

    #[test]
    fn token_upsert_preserves_refresh_token_when_callback_omits_it() {
        // Google only issues a refresh_token on first consent. A
        // subsequent refresh_token grant returns just an access_token
        // — we must NOT clobber the persisted refresh_token with NULL.
        let db = fresh_db();
        OauthClientStore::new(&db).upsert(&sample_client()).unwrap();
        let ts = OauthTokenStore::new(&db);
        ts.upsert(&OauthTokens {
            plugin_id: "plugin-google-contacts".into(),
            account_name: "controller".into(),
            access_token: "first".into(),
            refresh_token: Some("rt-1".into()),
            token_expires_at: 1_000,
            scopes_granted: "[\"x\"]".into(),
            account_email: Some("user@example.com".into()),
            created_at: 100,
            updated_at: 100,
        })
        .unwrap();
        // Refresh: access_token rotates, refresh_token absent.
        ts.upsert(&OauthTokens {
            plugin_id: "plugin-google-contacts".into(),
            account_name: "controller".into(),
            access_token: "second".into(),
            refresh_token: None,
            token_expires_at: 2_000,
            scopes_granted: "[\"x\"]".into(),
            account_email: None,
            created_at: 100,
            updated_at: 200,
        })
        .unwrap();
        let got = ts
            .get("plugin-google-contacts", "controller")
            .unwrap()
            .unwrap();
        assert_eq!(got.access_token, "second");
        assert_eq!(got.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(got.account_email.as_deref(), Some("user@example.com"));
        assert_eq!(got.token_expires_at, 2_000);
    }

    #[test]
    fn list_expiring_before_orders_earliest_first_and_caps_at_limit() {
        let db = fresh_db();
        OauthClientStore::new(&db).upsert(&sample_client()).unwrap();
        let mut c2 = sample_client();
        c2.account_name = "second".into();
        OauthClientStore::new(&db).upsert(&c2).unwrap();
        let ts = OauthTokenStore::new(&db);
        ts.upsert(&OauthTokens {
            plugin_id: "plugin-google-contacts".into(),
            account_name: "controller".into(),
            access_token: "a".into(),
            refresh_token: Some("rt".into()),
            token_expires_at: 5_000,
            scopes_granted: "[]".into(),
            account_email: None,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
        ts.upsert(&OauthTokens {
            plugin_id: "plugin-google-contacts".into(),
            account_name: "second".into(),
            access_token: "b".into(),
            refresh_token: Some("rt".into()),
            token_expires_at: 1_000,
            scopes_granted: "[]".into(),
            account_email: None,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
        // Ask for everything expiring on/before 6000, cap 1.
        let out = ts.list_expiring_before(6_000, 1).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].account_name, "second"); // earliest expiry
    }

    #[test]
    fn is_expiring_predicate_respects_slack() {
        let t = OauthTokens {
            plugin_id: "p".into(),
            account_name: "a".into(),
            access_token: "x".into(),
            refresh_token: None,
            token_expires_at: 1_000,
            scopes_granted: "[]".into(),
            account_email: None,
            created_at: 0,
            updated_at: 0,
        };
        assert!(!t.is_expiring(500, 0)); // 500s before expiry, no slack
        assert!(t.is_expiring(500, 600)); // 500s before, but slack of 600
        assert!(t.is_expiring(1_000, 0)); // exactly at expiry
        assert!(t.is_expiring(1_500, 0)); // past expiry
    }

    #[test]
    fn pending_consume_is_single_use_and_expiry_aware() {
        let db = fresh_db();
        let s = OauthPendingStore::new(&db);
        s.insert(&OauthPending {
            state_token: "csrf-1".into(),
            plugin_id: "plugin-google-contacts".into(),
            account_name: "controller".into(),
            redirect_to: Some("/settings/plugins/google-contacts".into()),
            created_at: 100,
            expires_at: 200,
        })
        .unwrap();
        // Within window: consume once.
        let got = s.consume("csrf-1", 150).unwrap();
        assert_eq!(got.plugin_id, "plugin-google-contacts");
        // Replay attempt: NoPending (the row is gone).
        let err = s.consume("csrf-1", 150).unwrap_err();
        assert!(matches!(err, OauthError::NoPending));
    }

    #[test]
    fn pending_consume_rejects_expired_row_without_deleting() {
        // Expired rows are skipped by consume (so they look like
        // NoPending) and removed by purge_expired. This test pins
        // the contract: an expired entry must not be silently
        // accepted, even if the state token matches.
        let db = fresh_db();
        let s = OauthPendingStore::new(&db);
        s.insert(&OauthPending {
            state_token: "stale".into(),
            plugin_id: "p".into(),
            account_name: "a".into(),
            redirect_to: None,
            created_at: 100,
            expires_at: 200,
        })
        .unwrap();
        let err = s.consume("stale", 500).unwrap_err();
        assert!(matches!(err, OauthError::NoPending));
        // The row is still present in the table — purge_expired
        // is what GCs it.
        assert_eq!(s.purge_expired(500).unwrap(), 1);
    }
}
