//! Operator-side `users` table — authentication identities for the
//! SPA + admin API.
//!
//! Distinct from `principals` (every participant the agent talks to,
//! resolved on inbound). A `User` row exists ONLY for someone who
//! logs into the admin surface.
//!
//! Today execlaw is single-controller by design (2026-04-23 locked
//! decision). The `users` table holds exactly one row in that mode.
//! Phase 7 hardening adds invite + role-scoped operators; the schema
//! is already shaped for it.
//!
//! `user_id` is the same value as the corresponding `principals.id`
//! row so the operator's auth identity and their participant identity
//! are unified — capability tokens minted for the operator carry the
//! same id whether they're logging into the admin UI or sending a
//! message in the chat thread.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Operator role. Single-user-controller mode uses `Controller`;
/// Phase 7+ adds `Operator` (write access, scoped) and `Viewer`
/// (read-only) without schema changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserRole {
    Controller,
    Operator,
    Viewer,
}

impl UserRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            UserRole::Controller => "controller",
            UserRole::Operator => "operator",
            UserRole::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "controller" => Some(Self::Controller),
            "operator" => Some(Self::Operator),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRow {
    pub user_id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub role: UserRole,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

pub struct UserStore<'db> {
    db: &'db Database,
}

impl<'db> UserStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a new user row. Returns `DbError::Invariant` if a row
    /// with the same `user_id` already exists — setup is supposed
    /// to be a one-shot operation.
    pub fn insert(&self, row: &UserRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO users \
                 (user_id, display_name, email, password_hash, role, created_at, last_login_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.user_id,
                    row.display_name,
                    row.email,
                    row.password_hash,
                    row.role.as_str(),
                    row.created_at,
                    row.last_login_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_by_id(&self, user_id: &str) -> Result<Option<UserRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT user_id, display_name, email, password_hash, role, \
                            created_at, last_login_at \
                     FROM users WHERE user_id = ?1",
                    params![user_id],
                    row_to_user,
                )
                .ok();
            Ok(got)
        })
    }

    /// Return the first (oldest by `created_at`) user. In
    /// single-controller mode this is THE controller; the SPA's
    /// "logged in as" affordance reads from this.
    pub fn get_first(&self) -> Result<Option<UserRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT user_id, display_name, email, password_hash, role, \
                            created_at, last_login_at \
                     FROM users ORDER BY created_at ASC LIMIT 1",
                    [],
                    row_to_user,
                )
                .ok();
            Ok(got)
        })
    }

    /// True when at least one user row exists. Used by `/api/ping`
    /// to decide between `pong` and `setup`.
    pub fn any_exist(&self) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
            Ok(n > 0)
        })
    }

    /// Stamp `last_login_at = now` on a successful login. Best-effort
    /// — failure here doesn't block login.
    pub fn touch_login(&self, user_id: &str, at: i64) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE users SET last_login_at = ?1 WHERE user_id = ?2",
                params![at, user_id],
            )?;
            Ok(())
        })
    }
}

fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    let role_str: String = r.get(4)?;
    Ok(UserRow {
        user_id: r.get(0)?,
        display_name: r.get(1)?,
        email: r.get(2)?,
        password_hash: r.get(3)?,
        role: UserRole::parse(&role_str).unwrap_or(UserRole::Controller),
        created_at: r.get(5)?,
        last_login_at: r.get(6)?,
    })
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

    fn mk_user(id: &str, role: UserRole) -> UserRow {
        UserRow {
            user_id: id.into(),
            display_name: "Test User".into(),
            email: Some(format!("{id}@example.com")),
            password_hash: "argon2-hash-here".into(),
            role,
            created_at: 100,
            last_login_at: None,
        }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        let u = mk_user("controller-1", UserRole::Controller);
        store.insert(&u).unwrap();
        let got = store.get_by_id("controller-1").unwrap().unwrap();
        assert_eq!(got.user_id, "controller-1");
        assert_eq!(got.role, UserRole::Controller);
        assert_eq!(got.email.as_deref(), Some("controller-1@example.com"));
    }

    #[test]
    fn any_exist_reflects_state() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        assert!(!store.any_exist().unwrap());
        store
            .insert(&mk_user("c1", UserRole::Controller))
            .unwrap();
        assert!(store.any_exist().unwrap());
    }

    #[test]
    fn duplicate_user_id_is_rejected() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        let u = mk_user("c1", UserRole::Controller);
        store.insert(&u).unwrap();
        // Second insert hits PRIMARY KEY violation.
        assert!(store.insert(&u).is_err());
    }

    #[test]
    fn get_first_returns_oldest_by_created_at() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        let mut early = mk_user("early", UserRole::Controller);
        early.created_at = 100;
        let mut late = mk_user("late", UserRole::Operator);
        late.created_at = 200;
        // Insert out of order.
        store.insert(&late).unwrap();
        store.insert(&early).unwrap();
        let first = store.get_first().unwrap().unwrap();
        assert_eq!(first.user_id, "early");
    }

    #[test]
    fn touch_login_updates_last_login_at() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        store
            .insert(&mk_user("c1", UserRole::Controller))
            .unwrap();
        store.touch_login("c1", 999).unwrap();
        let got = store.get_by_id("c1").unwrap().unwrap();
        assert_eq!(got.last_login_at, Some(999));
    }

    #[test]
    fn role_parse_and_str_round_trip() {
        for role in [UserRole::Controller, UserRole::Operator, UserRole::Viewer] {
            assert_eq!(UserRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(UserRole::parse("admin"), None);
    }
}
