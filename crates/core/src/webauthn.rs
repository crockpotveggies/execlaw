//! Persistence for registered WebAuthn credentials (Phase 7e).
//!
//! Each row pairs a user with a single passkey. `passkey_json` is the
//! opaque `webauthn_rs::Passkey` blob — execlaw never inspects it,
//! just round-trips it through the relying-party crate.
//!
//! Multiple credentials per user is fine and encouraged: an operator
//! who registers their YubiKey + their MacBook's Touch ID won't get
//! locked out if one device dies. The `count_for_user` helper drives
//! the login route's "does this user have webauthn at all?" branch.
//!
//! Counter handling: WebAuthn requires the relying party to reject
//! authentications with a non-monotonic signature counter (clones
//! the credential). `update_counter` is the only mutation path on the
//! counter field; it's strictly increasing per credential.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// One row in `state_webauthn_credentials`.
///
/// `credential_id` is the base64url-encoded raw id the authenticator
/// returned. `passkey_json` is `serde_json::to_string(&Passkey)` —
/// opaque from this crate's view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebauthnCredentialRow {
    pub credential_id: String,
    pub user_id: String,
    pub label: String,
    pub passkey_json: String,
    pub counter: i64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// View shape returned to the SPA — strips `passkey_json` since
/// it's an internal blob, never useful in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebauthnCredentialSummary {
    pub credential_id: String,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl From<&WebauthnCredentialRow> for WebauthnCredentialSummary {
    fn from(row: &WebauthnCredentialRow) -> Self {
        Self {
            credential_id: row.credential_id.clone(),
            label: row.label.clone(),
            created_at: row.created_at,
            last_used_at: row.last_used_at,
        }
    }
}

/// Maximum number of credentials per user. Keeps the per-login
/// `start_passkey_authentication` candidate-list bounded so a
/// pathological registrant can't blow up the challenge payload.
pub const MAX_CREDENTIALS_PER_USER: usize = 10;

pub struct WebauthnStore<'db> {
    db: &'db Database,
}

impl<'db> WebauthnStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a freshly-registered credential. Returns `Err` if the
    /// `credential_id` is already registered (PRIMARY KEY collision)
    /// or if the user has hit `MAX_CREDENTIALS_PER_USER`.
    pub fn insert(&self, row: &WebauthnCredentialRow) -> Result<(), DbError> {
        // Cap registrations per user. Done inside the same conn so the
        // count + insert are consistent under SQLite's serialised
        // writer (no separate tx needed for a per-user cap that is
        // advisory under serialised access).
        self.db.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM state_webauthn_credentials WHERE user_id = ?1",
                params![row.user_id],
                |r| r.get(0),
            )?;
            if (n as usize) >= MAX_CREDENTIALS_PER_USER {
                return Err(DbError::Config(format!(
                    "user {} has reached the {}-credential cap",
                    row.user_id, MAX_CREDENTIALS_PER_USER
                )));
            }
            c.execute(
                "INSERT INTO state_webauthn_credentials \
                 (credential_id, user_id, label, passkey_json, counter, \
                  created_at, last_used_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.credential_id,
                    row.user_id,
                    row.label,
                    row.passkey_json,
                    row.counter,
                    row.created_at,
                    row.last_used_at,
                ],
            )?;
            Ok(())
        })
    }

    /// Count credentials for a user. Powers the login-route branch:
    /// `> 0` means "demand a webauthn assertion after the password
    /// check succeeds."
    pub fn count_for_user(&self, user_id: &str) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM state_webauthn_credentials WHERE user_id = ?1",
                params![user_id],
                |r| r.get(0),
            )?;
            Ok(n as usize)
        })
    }

    /// Fetch every row for a user (full state including the opaque
    /// passkey blob). Used by the assertion ceremony to rebuild the
    /// candidate list passed to `start_passkey_authentication`.
    pub fn list_for_user(&self, user_id: &str) -> Result<Vec<WebauthnCredentialRow>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT credential_id, user_id, label, passkey_json, counter, \
                        created_at, last_used_at \
                 FROM state_webauthn_credentials \
                 WHERE user_id = ?1 \
                 ORDER BY created_at ASC, credential_id ASC",
            )?;
            let rows = stmt
                .query_map(params![user_id], row_to_credential)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// One-shot lookup by credential_id. Used during finish-auth to
    /// pull the single matching row + bump its counter + last-used.
    pub fn get(&self, credential_id: &str) -> Result<Option<WebauthnCredentialRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT credential_id, user_id, label, passkey_json, counter, \
                            created_at, last_used_at \
                     FROM state_webauthn_credentials WHERE credential_id = ?1",
                    params![credential_id],
                    row_to_credential,
                )
                .ok();
            Ok(got)
        })
    }

    /// Bump the signature counter + stamp last_used_at after a
    /// successful authentication. The counter is monotonic per
    /// WebAuthn spec; the caller must verify `new_counter > old_counter`
    /// (or that the authenticator reports zero, which spec allows).
    pub fn update_counter(
        &self,
        credential_id: &str,
        new_counter: i64,
        last_used_at: i64,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_webauthn_credentials \
                 SET counter = ?1, last_used_at = ?2 \
                 WHERE credential_id = ?3",
                params![new_counter, last_used_at, credential_id],
            )?;
            Ok(())
        })
    }

    /// Remove a credential. Returns `true` when a row was deleted,
    /// `false` if the credential_id wasn't present (caller can decide
    /// whether to surface 404 or silently succeed).
    pub fn delete(&self, credential_id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_webauthn_credentials WHERE credential_id = ?1",
                params![credential_id],
            )?;
            Ok(n > 0)
        })
    }

    /// Strict variant: only deletes when the credential belongs to
    /// the supplied user_id. Powers the route's per-user authorization
    /// — controller A must not be able to remove controller B's keys.
    pub fn delete_owned(&self, credential_id: &str, user_id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM state_webauthn_credentials \
                 WHERE credential_id = ?1 AND user_id = ?2",
                params![credential_id, user_id],
            )?;
            Ok(n > 0)
        })
    }
}

fn row_to_credential(row: &rusqlite::Row<'_>) -> rusqlite::Result<WebauthnCredentialRow> {
    Ok(WebauthnCredentialRow {
        credential_id: row.get(0)?,
        user_id: row.get(1)?,
        label: row.get(2)?,
        passkey_json: row.get(3)?,
        counter: row.get(4)?,
        created_at: row.get(5)?,
        last_used_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        // Insert the parent user so the FK is satisfied.
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO users \
                 (user_id, username, display_name, email, password_hash, role, \
                  created_at, last_login_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, NULL)",
                params!["u1", "alice", "Alice", "argon2-hash", "controller", 0i64],
            )?;
            Ok(())
        })
        .unwrap();
        db
    }

    fn mk_row(credential_id: &str, user_id: &str, label: &str) -> WebauthnCredentialRow {
        WebauthnCredentialRow {
            credential_id: credential_id.into(),
            user_id: user_id.into(),
            label: label.into(),
            passkey_json: r#"{"opaque":"blob"}"#.into(),
            counter: 0,
            created_at: 100,
            last_used_at: None,
        }
    }

    #[test]
    fn insert_and_count_roundtrip() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        assert_eq!(store.count_for_user("u1").unwrap(), 0);
        store.insert(&mk_row("cred-a", "u1", "yubikey-5c")).unwrap();
        store.insert(&mk_row("cred-b", "u1", "macbook")).unwrap();
        assert_eq!(store.count_for_user("u1").unwrap(), 2);
        assert_eq!(store.count_for_user("nope").unwrap(), 0);
    }

    #[test]
    fn list_for_user_orders_by_created_at_then_id() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        let mut a = mk_row("cred-a", "u1", "older");
        a.created_at = 50;
        let mut b = mk_row("cred-b", "u1", "newer");
        b.created_at = 200;
        let mut c = mk_row("cred-c", "u1", "newer-dup");
        c.created_at = 200;
        store.insert(&b).unwrap();
        store.insert(&c).unwrap();
        store.insert(&a).unwrap();
        let listed = store.list_for_user("u1").unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].credential_id, "cred-a");
        assert_eq!(listed[1].credential_id, "cred-b");
        assert_eq!(listed[2].credential_id, "cred-c");
    }

    #[test]
    fn duplicate_credential_id_rejected() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        store.insert(&mk_row("cred-a", "u1", "first")).unwrap();
        let err = store.insert(&mk_row("cred-a", "u1", "second"));
        assert!(err.is_err(), "PK collision must surface as Err");
    }

    #[test]
    fn cap_enforced_at_max_per_user() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        for i in 0..MAX_CREDENTIALS_PER_USER {
            store
                .insert(&mk_row(&format!("cred-{i}"), "u1", "k"))
                .unwrap();
        }
        let err = store.insert(&mk_row("cred-overflow", "u1", "k"));
        assert!(matches!(err, Err(DbError::Config(_))), "cap returns Config error, not Sqlite");
    }

    #[test]
    fn update_counter_bumps_value_and_last_used() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        store.insert(&mk_row("cred-a", "u1", "k")).unwrap();
        store.update_counter("cred-a", 17, 999_000).unwrap();
        let row = store.get("cred-a").unwrap().unwrap();
        assert_eq!(row.counter, 17);
        assert_eq!(row.last_used_at, Some(999_000));
    }

    #[test]
    fn delete_returns_true_then_false() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        store.insert(&mk_row("cred-a", "u1", "k")).unwrap();
        assert!(store.delete("cred-a").unwrap());
        assert!(!store.delete("cred-a").unwrap());
        assert_eq!(store.count_for_user("u1").unwrap(), 0);
    }

    #[test]
    fn delete_owned_refuses_other_users_credential() {
        // Provision a second user so the FK on a "wrong owner" delete
        // attempt has somewhere to point.
        let db = fresh_db();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO users \
                 (user_id, username, display_name, email, password_hash, role, \
                  created_at, last_login_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, NULL)",
                params!["u2", "bob", "Bob", "argon2-hash", "controller", 0i64],
            )?;
            Ok(())
        })
        .unwrap();
        let store = WebauthnStore::new(&db);
        store.insert(&mk_row("cred-a", "u1", "k")).unwrap();
        // bob tries to delete alice's credential — must be a no-op.
        assert!(!store.delete_owned("cred-a", "u2").unwrap());
        // Still there.
        assert!(store.get("cred-a").unwrap().is_some());
        // Owner delete works.
        assert!(store.delete_owned("cred-a", "u1").unwrap());
    }

    #[test]
    fn cascade_delete_when_user_removed() {
        let db = fresh_db();
        let store = WebauthnStore::new(&db);
        store.insert(&mk_row("cred-a", "u1", "k")).unwrap();
        store.insert(&mk_row("cred-b", "u1", "k")).unwrap();
        assert_eq!(store.count_for_user("u1").unwrap(), 2);
        // Drop the user; FK ON DELETE CASCADE must remove credentials.
        // Note: SQLite needs PRAGMA foreign_keys=ON, which Database
        // sets by default in DbConfig.
        db.with_conn(|c| {
            c.execute("DELETE FROM users WHERE user_id = ?1", params!["u1"])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(store.count_for_user("u1").unwrap(), 0);
    }
}
