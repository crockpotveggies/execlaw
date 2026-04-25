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

/// Minimum username length. Three lets a controller pick a short
/// handle ("jl", "ops") while keeping accidental empty/garbage submissions
/// out.
pub const USERNAME_MIN_LEN: usize = 3;
/// Maximum username length — generous; the login form is a single field.
pub const USERNAME_MAX_LEN: usize = 32;

/// Validate a raw username (post-trim). Returns the canonical
/// (lowercased) form on success.
///
/// Allowed: ASCII lowercase letters, digits, underscore, hyphen.
/// Length: [USERNAME_MIN_LEN, USERNAME_MAX_LEN].
///
/// We require an explicit username so the controller's login handle
/// is decoupled from the agent-facing display name. The display name
/// is "Justin Long"; the username is something like "jlong".
pub fn normalize_username(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("username is required");
    }
    let lower = trimmed.to_ascii_lowercase();
    let chars = lower.chars().count();
    if chars < USERNAME_MIN_LEN {
        return Err("username must be at least 3 characters");
    }
    if chars > USERNAME_MAX_LEN {
        return Err("username must be at most 32 characters");
    }
    if !lower
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("username may only contain letters, digits, underscore, hyphen");
    }
    Ok(lower)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRow {
    pub user_id: String,
    /// Login handle. Always lowercased + validated; see [`normalize_username`].
    pub username: String,
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

    /// Insert a new user row. Returns `DbError::Sqlite` if a row
    /// with the same `user_id` or `username` already exists — setup
    /// is supposed to be a one-shot operation, and usernames must
    /// be unique.
    pub fn insert(&self, row: &UserRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO users \
                 (user_id, username, display_name, email, password_hash, role, \
                  created_at, last_login_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.user_id,
                    row.username,
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
                    "SELECT user_id, username, display_name, email, password_hash, role, \
                            created_at, last_login_at \
                     FROM users WHERE user_id = ?1",
                    params![user_id],
                    row_to_user,
                )
                .ok();
            Ok(got)
        })
    }

    /// Look up a user by their (already-normalized, lowercased) username.
    /// Returns None on miss — login routes turn that into "bad credentials"
    /// rather than 404 to avoid leaking which usernames exist.
    pub fn get_by_username(&self, username: &str) -> Result<Option<UserRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT user_id, username, display_name, email, password_hash, role, \
                            created_at, last_login_at \
                     FROM users WHERE username = ?1",
                    params![username],
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
                    "SELECT user_id, username, display_name, email, password_hash, role, \
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

    /// Replace a user's password hash. Caller is responsible for
    /// having already verified whatever proof-of-identity policy
    /// applies (current-password match for self-change, Controller
    /// auth for admin reset). Returns `true` when a row was actually
    /// updated.
    pub fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE users SET password_hash = ?1 WHERE user_id = ?2",
                params![hash, user_id],
            )?;
            Ok(n > 0)
        })
    }

    /// Every user row, oldest first (matches the SPA's expectation:
    /// the controller is row 0 + invitees follow). Phase-7 multi-
    /// controller surface: `GET /api/admin/users`.
    pub fn list_all(&self) -> Result<Vec<UserRow>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT user_id, username, display_name, email, password_hash, role, \
                        created_at, last_login_at \
                 FROM users ORDER BY created_at ASC, user_id ASC",
            )?;
            let rows = stmt
                .query_map([], row_to_user)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Delete a user by id. Returns `true` if a row was removed,
    /// `false` if the id was unknown.
    pub fn delete(&self, user_id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute("DELETE FROM users WHERE user_id = ?1", params![user_id])?;
            Ok(n > 0)
        })
    }

    /// Count rows with the given role. Used by the Phase-7 invite
    /// route to enforce "at least one controller must remain" on
    /// delete.
    pub fn count_by_role(&self, role: UserRole) -> Result<usize, DbError> {
        self.db.with_conn(|c| {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM users WHERE role = ?1",
                    params![role.as_str()],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok(n as usize)
        })
    }
}

fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    let role_str: String = r.get(5)?;
    Ok(UserRow {
        user_id: r.get(0)?,
        username: r.get(1)?,
        display_name: r.get(2)?,
        email: r.get(3)?,
        password_hash: r.get(4)?,
        role: UserRole::parse(&role_str).unwrap_or(UserRole::Controller),
        created_at: r.get(6)?,
        last_login_at: r.get(7)?,
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
            username: id.replace(['-', '_'], "").to_lowercase(),
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

    // ---- list_all + delete + count_by_role (Phase 7 multi-user) -----

    #[test]
    fn list_all_orders_oldest_first() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        let mut early = mk_user("c1", UserRole::Controller);
        early.created_at = 100;
        let mut mid = mk_user("op1", UserRole::Operator);
        mid.created_at = 200;
        let mut late = mk_user("v1", UserRole::Viewer);
        late.created_at = 300;
        // Insert out of order.
        store.insert(&late).unwrap();
        store.insert(&early).unwrap();
        store.insert(&mid).unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(
            all.iter().map(|u| u.user_id.as_str()).collect::<Vec<_>>(),
            vec!["c1", "op1", "v1"],
        );
    }

    #[test]
    fn delete_returns_false_for_unknown_id() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        assert!(!store.delete("missing").unwrap());
        store.insert(&mk_user("c1", UserRole::Controller)).unwrap();
        assert!(store.delete("c1").unwrap());
        // Repeating returns false (idempotent observability).
        assert!(!store.delete("c1").unwrap());
        assert!(store.list_all().unwrap().is_empty());
    }

    #[test]
    fn set_password_hash_updates_row_and_returns_true_then_false() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        store.insert(&mk_user("c1", UserRole::Controller)).unwrap();
        assert!(store.set_password_hash("c1", "new-argon2-hash").unwrap());
        let row = store.get_by_id("c1").unwrap().unwrap();
        assert_eq!(row.password_hash, "new-argon2-hash");
        // Unknown user → false, no error.
        assert!(!store.set_password_hash("nobody", "x").unwrap());
    }

    #[test]
    fn count_by_role_reflects_population() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        store.insert(&mk_user("c1", UserRole::Controller)).unwrap();
        store.insert(&mk_user("c2", UserRole::Controller)).unwrap();
        store.insert(&mk_user("o1", UserRole::Operator)).unwrap();
        assert_eq!(store.count_by_role(UserRole::Controller).unwrap(), 2);
        assert_eq!(store.count_by_role(UserRole::Operator).unwrap(), 1);
        assert_eq!(store.count_by_role(UserRole::Viewer).unwrap(), 0);
    }

    // ---- normalize_username -------------------------------------------

    #[test]
    fn normalize_username_lowercases_and_trims() {
        assert_eq!(normalize_username("  JLong  ").unwrap(), "jlong");
        assert_eq!(normalize_username("Justin").unwrap(), "justin");
    }

    #[test]
    fn normalize_username_rejects_empty_and_too_short() {
        assert!(normalize_username("").is_err());
        assert!(normalize_username("   ").is_err());
        assert!(normalize_username("ab").is_err());
        // 3 chars is the boundary — must succeed.
        assert_eq!(normalize_username("abc").unwrap(), "abc");
    }

    #[test]
    fn normalize_username_rejects_too_long() {
        let max_ok = "a".repeat(USERNAME_MAX_LEN);
        assert!(normalize_username(&max_ok).is_ok());
        let too_long = "a".repeat(USERNAME_MAX_LEN + 1);
        assert!(normalize_username(&too_long).is_err());
    }

    #[test]
    fn normalize_username_rejects_disallowed_characters() {
        for bad in ["has space", "j@l", "with.dot", "ümlaut", "with/slash", "emo😀ji"] {
            assert!(
                normalize_username(bad).is_err(),
                "should reject '{bad}'"
            );
        }
        // Allowed forms.
        for ok in ["jlong", "j_long", "j-long", "user42"] {
            assert!(
                normalize_username(ok).is_ok(),
                "should accept '{ok}'"
            );
        }
    }

    // ---- username persistence + lookup -------------------------------

    #[test]
    fn get_by_username_returns_inserted_row() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        let mut u = mk_user("controller-1", UserRole::Controller);
        u.username = "jlong".into();
        store.insert(&u).unwrap();
        let got = store.get_by_username("jlong").unwrap().unwrap();
        assert_eq!(got.user_id, "controller-1");
        assert_eq!(got.username, "jlong");
        // Miss returns None.
        assert!(store.get_by_username("nobody").unwrap().is_none());
    }

    /// Adversarial: two rows with the same username trip the unique
    /// index — even if user_id differs. Phase-7 multi-user adds the
    /// invite flow on top of this guarantee.
    #[test]
    fn duplicate_username_is_rejected_by_unique_index() {
        let db = fresh_db();
        let store = UserStore::new(&db);
        let mut a = mk_user("c1", UserRole::Controller);
        a.username = "shared".into();
        let mut b = mk_user("c2", UserRole::Operator);
        b.username = "shared".into();
        store.insert(&a).unwrap();
        let err = store.insert(&b).unwrap_err();
        // SQLite raises a UNIQUE constraint violation; we just need
        // the insert to fail.
        assert!(matches!(err, DbError::Sqlite(_)), "got {err:?}");
    }

    /// Round-trip: insert with mixed-case raw input → read back →
    /// must be the lowercased form (the caller is responsible for
    /// running normalize_username before insert; we don't do it here
    /// to keep the store layer thin, but this test documents the
    /// expectation that both write paths use the helper).
    #[test]
    fn username_roundtrips_lowercased() {
        let lowered = normalize_username("JLong").unwrap();
        let db = fresh_db();
        let store = UserStore::new(&db);
        let mut u = mk_user("c1", UserRole::Controller);
        u.username = lowered.clone();
        store.insert(&u).unwrap();
        let got = store.get_by_username(&lowered).unwrap().unwrap();
        assert_eq!(got.username, "jlong");
    }
}
