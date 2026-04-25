//! Persistent refresh-token store (Phase 7 hardening).
//!
//! Backs `state_refresh_tokens` with the same `(token, principal_id,
//! session_id, issued_at, expires_at)` shape the in-memory `DashMap`
//! used. The behaviour is deliberately identical to the prior
//! implementation:
//!
//!   - `issue` mints a new token bound to a (principal, session) pair
//!     and inserts the row.
//!   - `consume` is single-use: it `DELETE ... RETURNING`s the row
//!     and returns `None` if missing OR expired.
//!   - `revoke_session` deletes every row sharing a `session_id` so
//!     a logout invalidates every rotation since that session began.
//!   - `revoke_all_for_user` deletes every row for a `principal_id`
//!     so "sign out everywhere" actually does.
//!
//! Token strings stay UUID-v4 pairs — opaque to the caller. The
//! database layer is the only one that knows how to rotate them.

use crate::db::{Database, DbError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Minted from `consume` — the same shape the in-memory store
/// produced so the route layer can stay agnostic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshTokenRow {
    pub principal_id: String,
    pub session_id: String,
    pub expires_at: i64,
}

pub struct RefreshTokenStore<'db> {
    db: &'db Database,
}

impl<'db> RefreshTokenStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Issue a new refresh token. The string itself is opaque (two
    /// concatenated UUID-v4s, ~72 chars) so collisions are infeasible
    /// even before the DB unique constraint catches them.
    pub fn issue(
        &self,
        principal_id: &str,
        session_id: &str,
        ttl_secs: i64,
    ) -> Result<String, DbError> {
        let now = chrono::Utc::now().timestamp();
        let token = format!("{}-{}", Uuid::new_v4(), Uuid::new_v4());
        let expires_at = now + ttl_secs;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_refresh_tokens \
                 (token, principal_id, session_id, issued_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token, principal_id, session_id, now, expires_at],
            )?;
            Ok(())
        })?;
        Ok(token)
    }

    /// Single-use consumption. The row is deleted on read; a second
    /// call with the same token returns `None`. Expired tokens are
    /// also returned as `None` (and the row is purged so the caller
    /// can't replay them).
    pub fn consume(&self, token: &str) -> Result<Option<RefreshTokenRow>, DbError> {
        let now = chrono::Utc::now().timestamp();
        self.db.with_conn(|c| {
            // Atomic DELETE … RETURNING avoids the read-then-delete
            // race that the in-memory implementation didn't need to
            // care about.
            let row: Option<(String, String, i64)> = c
                .query_row(
                    "DELETE FROM state_refresh_tokens \
                     WHERE token = ?1 \
                     RETURNING principal_id, session_id, expires_at",
                    params![token],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let (principal_id, session_id, expires_at) = match row {
                Some(t) => t,
                None => return Ok(None),
            };
            if now > expires_at {
                return Ok(None);
            }
            Ok(Some(RefreshTokenRow {
                principal_id,
                session_id,
                expires_at,
            }))
        })
    }

    /// Revoke every refresh token sharing the given `session_id`.
    /// Returns the number of rows deleted.
    pub fn revoke_session(&self, session_id: &str) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_refresh_tokens WHERE session_id = ?1",
                params![session_id],
            )?;
            Ok(n)
        })
    }

    /// Revoke every refresh token for a user. Used by the
    /// "sign out everywhere" route: tells the browser the operator
    /// has zero live sessions left, anywhere.
    pub fn revoke_all_for_user(&self, principal_id: &str) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_refresh_tokens WHERE principal_id = ?1",
                params![principal_id],
            )?;
            Ok(n)
        })
    }

    /// Count active (non-expired) sessions for a user. Drives the
    /// admin "active sessions" surface.
    pub fn active_session_count(&self, principal_id: &str) -> Result<usize, DbError> {
        let now = chrono::Utc::now().timestamp();
        self.db.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(DISTINCT session_id) \
                 FROM state_refresh_tokens \
                 WHERE principal_id = ?1 AND expires_at > ?2",
                params![principal_id, now],
                |r| r.get(0),
            )?;
            Ok(n as usize)
        })
    }

    /// Sweep every expired row. Cheap (no triggers, no FK cascades);
    /// safe to call on a periodic timer. Returns rows removed.
    pub fn purge_expired(&self) -> Result<usize, DbError> {
        let now = chrono::Utc::now().timestamp();
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_refresh_tokens WHERE expires_at <= ?1",
                params![now],
            )?;
            Ok(n)
        })
    }
}

// ---------------------------------------------------------------------------
// Periodic sweeper — drives `purge_expired` on a tokio interval.
// Mirrors `LogRetentionSweeper` so operators reason about every
// background sweeper with the same mental model.
// ---------------------------------------------------------------------------

/// Default sweep cadence. Hourly is plenty — refresh tokens are
/// already rejected at consume time when expired, so the sweeper's
/// only job is keeping `state_refresh_tokens` from growing without
/// bound across many issue/expire cycles.
pub const REFRESH_TOKEN_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct RefreshTokenSweeper {
    db: Database,
    interval: Duration,
    kick: Arc<Notify>,
}

impl RefreshTokenSweeper {
    pub fn new(db: Database) -> Self {
        Self::with_interval(db, REFRESH_TOKEN_SWEEP_INTERVAL)
    }

    pub fn with_interval(db: Database, interval: Duration) -> Self {
        Self {
            db,
            interval,
            kick: Arc::new(Notify::new()),
        }
    }

    /// Force the run loop to sweep now. Coalesces — multiple kicks
    /// while the loop is busy collapse into one extra sweep.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Drive the sweep loop until `stop` is notified. Mirrors
    /// `LogRetentionSweeper::run` for consistency.
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "refresh-token sweeper running"
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("refresh-token sweeper stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "refresh-token sweep failed; will retry next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<usize, DbError> {
        let n = RefreshTokenStore::new(&self.db).purge_expired()?;
        if n > 0 {
            debug!(rows = n, "refresh-token sweep");
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn issue_and_consume_roundtrip() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let tok = store.issue("user-1", "session-1", 3600).unwrap();
        let row = store.consume(&tok).unwrap().unwrap();
        assert_eq!(row.principal_id, "user-1");
        assert_eq!(row.session_id, "session-1");
        assert!(row.expires_at > chrono::Utc::now().timestamp());
    }

    #[test]
    fn consume_is_single_use() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let tok = store.issue("u", "s", 3600).unwrap();
        assert!(store.consume(&tok).unwrap().is_some());
        assert!(
            store.consume(&tok).unwrap().is_none(),
            "second consume must miss — row was deleted on first read",
        );
    }

    #[test]
    fn expired_token_is_treated_as_missing() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let tok = store.issue("u", "s", -10).unwrap(); // already expired
        // The row IS present, but consume returns None because the
        // expiry check fails. We still drop it so it can't be replayed.
        assert!(store.consume(&tok).unwrap().is_none());
    }

    #[test]
    fn revoke_session_kills_every_token_for_that_session() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let t1 = store.issue("u", "sess-A", 3600).unwrap();
        let t2 = store.issue("u", "sess-A", 3600).unwrap();
        let t3 = store.issue("u", "sess-B", 3600).unwrap();
        let removed = store.revoke_session("sess-A").unwrap();
        assert_eq!(removed, 2);
        assert!(store.consume(&t1).unwrap().is_none());
        assert!(store.consume(&t2).unwrap().is_none());
        // sess-B's token survives.
        assert!(store.consume(&t3).unwrap().is_some());
    }

    #[test]
    fn revoke_all_for_user_kills_every_session() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let _ = store.issue("alice", "sess-A", 3600).unwrap();
        let _ = store.issue("alice", "sess-B", 3600).unwrap();
        let bob_tok = store.issue("bob", "sess-X", 3600).unwrap();
        let removed = store.revoke_all_for_user("alice").unwrap();
        assert_eq!(removed, 2);
        // Alice has zero rows.
        assert_eq!(store.active_session_count("alice").unwrap(), 0);
        // Bob's session survives.
        assert!(store.consume(&bob_tok).unwrap().is_some());
    }

    #[test]
    fn active_session_count_distinct_by_session_id() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        // Two rotations within the same session count as ONE session.
        let _ = store.issue("u", "sess-A", 3600).unwrap();
        let _ = store.issue("u", "sess-A", 3600).unwrap();
        let _ = store.issue("u", "sess-B", 3600).unwrap();
        assert_eq!(store.active_session_count("u").unwrap(), 2);
    }

    #[test]
    fn purge_expired_drops_only_old_rows() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let _ = store.issue("u", "s", -10).unwrap();
        let live = store.issue("u", "s", 3600).unwrap();
        let removed = store.purge_expired().unwrap();
        assert_eq!(removed, 1);
        assert!(
            store.consume(&live).unwrap().is_some(),
            "live token must survive the purge",
        );
    }

    #[test]
    fn unknown_token_returns_none() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        assert!(store.consume("not-a-real-token").unwrap().is_none());
    }

    #[tokio::test]
    async fn sweeper_run_loop_purges_then_stops_on_signal() {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        let _expired = store.issue("u", "s", -10).unwrap();
        let live = store.issue("u", "s", 3600).unwrap();

        let sweeper = RefreshTokenSweeper::with_interval(
            db.clone(),
            Duration::from_millis(20),
        );
        let stop = Arc::new(Notify::new());
        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });

        sweeper.kick();
        tokio::time::sleep(Duration::from_millis(60)).await;
        stop.notify_one();
        handle.await.unwrap();

        // The expired row is gone; the live token still consumes.
        assert!(
            store.consume(&live).unwrap().is_some(),
            "live token must survive the sweeper",
        );
    }
}
