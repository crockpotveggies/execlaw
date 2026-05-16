//! SQLite + SQLCipher connection management.
//!
//! Per MIGRATION_PLAN §6.3 / §6.4:
//!
//! - Whole `execlaw.db` is SQLCipher-encrypted; master key comes from the OS
//!   keyring (or a passphrase-file fallback for headless hosts).
//! - Every connection runs `PRAGMA key = '...'` before any SQL.
//! - `PRAGMA journal_mode = WAL` so readers don't block the writer.
//! - `PRAGMA foreign_keys = ON` **per connection** (the default is OFF in
//!   SQLite and is easy to forget — so we enforce it here).
//! - `PRAGMA synchronous = NORMAL` for the main DB (good durability/speed
//!   tradeoff with WAL). The vault tables live in the same DB, so the whole
//!   file is encrypted; operator can re-key with `execlaw vault rekey`.

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// Configuration for opening the database.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Path to the SQLCipher-encrypted DB file.
    pub path: PathBuf,
    /// Raw master key. If `None`, the DB is opened as **plaintext SQLite**
    /// (only permitted for unit tests — production always has a key).
    ///
    /// Expected shape for production: a hex-encoded 32-byte key — rusqlite
    /// accepts it via `PRAGMA key = "x'...'";`. For simplicity the caller
    /// can also pass a passphrase and we'll use it directly.
    pub key: Option<SqlCipherKey>,
}

impl DbConfig {
    /// Config for an ephemeral in-memory database used by unit tests.
    /// SQLCipher-key-less — acceptable ONLY in `#[cfg(test)]`.
    pub fn in_memory_unencrypted() -> Self {
        Self {
            path: PathBuf::from(":memory:"),
            key: None,
        }
    }
}

/// Key material for SQLCipher.
///
/// Prefer `RawBytes(32)` in production (hex-escaped when we invoke `PRAGMA`).
/// `Passphrase` is offered as a convenience for the headless-fallback flow
/// described in §6.4.
#[derive(Clone)]
pub enum SqlCipherKey {
    /// 32 raw bytes. Will be hex-escaped in the PRAGMA statement.
    RawBytes(Vec<u8>),
    /// Human-entered passphrase (SQLCipher derives the key via PBKDF2).
    Passphrase(String),
}

impl std::fmt::Debug for SqlCipherKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlCipherKey::RawBytes(b) => f
                .debug_tuple("RawBytes")
                .field(&format!("<{} bytes>", b.len()))
                .finish(),
            SqlCipherKey::Passphrase(_) => {
                f.debug_tuple("Passphrase").field(&"<redacted>").finish()
            }
        }
    }
}

/// Top-level errors from this crate's database layer.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database configuration error: {0}")]
    Config(String),
    #[error("migration error: {0}")]
    Migration(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event log invariant violated: {0}")]
    Invariant(String),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("event log tamper detected: {0}")]
    TamperDetected(String),
}

/// Thin wrapper around a single writer-thread SQLite connection.
///
/// The writer-per-DB pattern from §6.3: one writer, many readers. For the
/// scope of Phase 0 we keep a single `Arc<Mutex<Connection>>` and let
/// readers share it — simpler than a pool and adequate for the kind of
/// workload the control plane generates. We can swap in `r2d2` later
/// without changing consumers.
#[derive(Clone)]
pub struct Database {
    inner: Arc<std::sync::Mutex<Connection>>,
    path: PathBuf,
}

impl Database {
    /// Open (or create) the database at the given config.
    ///
    /// Applies `PRAGMA key` (when `key` is Some), `journal_mode = WAL`,
    /// `foreign_keys = ON`, and `synchronous = NORMAL`.
    pub fn open(config: &DbConfig) -> Result<Self, DbError> {
        let conn = if config.path == Path::new(":memory:") {
            Connection::open_in_memory()?
        } else {
            if let Some(parent) = config.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Connection::open(&config.path)?
        };

        Self::apply_init_pragmas(&conn, config)?;

        Ok(Self {
            inner: Arc::new(std::sync::Mutex::new(conn)),
            path: config.path.clone(),
        })
    }

    fn apply_init_pragmas(conn: &Connection, config: &DbConfig) -> Result<(), DbError> {
        // Key BEFORE anything else — SQLCipher requires the first statement
        // on a new connection to set the key, otherwise reads return corrupt
        // data.
        if let Some(key) = &config.key {
            match key {
                SqlCipherKey::RawBytes(bytes) => {
                    if bytes.len() != 32 {
                        return Err(DbError::Config(format!(
                            "expected 32 raw key bytes, got {}",
                            bytes.len()
                        )));
                    }
                    let hex = hex::encode(bytes);
                    // Use `x'...'` blob literal form so SQLCipher treats this
                    // as raw key material and skips KDF.
                    let pragma = format!("PRAGMA key = \"x'{}'\";", hex);
                    conn.execute_batch(&pragma)?;
                }
                SqlCipherKey::Passphrase(pass) => {
                    // rusqlite offers `pragma_update` but it double-quotes the
                    // value; we need single-quote escaping here to match
                    // SQLCipher's expected input.
                    let escaped = pass.replace('\'', "''");
                    let pragma = format!("PRAGMA key = '{}';", escaped);
                    conn.execute_batch(&pragma)?;
                }
            }
        }

        // WAL — readers don't block the writer.
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Foreign keys default OFF in SQLite; force ON every connection.
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // NORMAL is the right tradeoff with WAL (§6.3).
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        Ok(())
    }

    /// Path the DB was opened at (for log/display purposes).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Execute a closure holding the connection lock.
    ///
    /// Every caller *must* keep the closure short — we're single-writer.
    pub fn with_conn<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Connection) -> Result<R, DbError>,
    {
        let guard = self
            .inner
            .lock()
            .map_err(|e| DbError::Config(format!("connection mutex poisoned: {e}")))?;
        f(&guard)
    }

    /// Execute a transactional closure.
    pub fn transaction<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<R, DbError>,
    {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| DbError::Config(format!("connection mutex poisoned: {e}")))?;
        let tx = guard.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Read a single PRAGMA value as text. Intended for sanity tests.
    pub fn pragma_text(&self, name: &str) -> Result<String, DbError> {
        self.with_conn(|c| {
            let mut got = String::new();
            c.pragma_query(None, name, |row| {
                got = row.get::<_, String>(0).unwrap_or_default();
                Ok(())
            })?;
            Ok(got)
        })
    }

    /// Wipe the database back to first-boot state by closing the
    /// current connection, deleting the file(s) on disk, and opening
    /// a fresh blank one at the same path with the same encryption
    /// posture. **The schema is gone** — callers MUST run
    /// `MigrationRunner::apply_all()` afterwards to re-create tables
    /// and re-fire migration seeds.
    ///
    /// Why this is preferable to schema-level `DROP TABLE` loops:
    ///
    ///   * Virtual tables (FTS5 `skill_search`, etc.) require the
    ///     vtable module to load successfully at DROP time. A
    ///     mismatched SQLite/SQLCipher build, a missing extension, or
    ///     simply a connection that was opened with different module
    ///     registrations than at `CREATE VIRTUAL TABLE` time will
    ///     surface as `vtable constructor failed: <name>` — observed
    ///     in production today (2026-05-13). File-level delete bypasses
    ///     the vtable module entirely.
    ///   * `sqlite_master` enumeration + `DROP TABLE` is sensitive to
    ///     FK declaration order: SQLite refuses to drop a parent that
    ///     a child still references. Even with `foreign_keys = OFF`
    ///     some configurations trip on this. File delete sidesteps
    ///     the dependency graph.
    ///   * Plain semantic: "factory reset" should mean "the DB equals
    ///     what `execlaw install` would have produced." Deleting the
    ///     file then re-opening is exactly that.
    ///
    /// On `:memory:` paths the file-delete is skipped; the in-memory
    /// connection is simply replaced with a fresh in-memory one. Used
    /// by the test harness.
    ///
    /// Side-files cleaned up alongside the main DB file:
    ///
    ///   * `<path>-wal` — write-ahead log (always present in WAL mode)
    ///   * `<path>-shm` — shared-memory file backing the WAL
    ///   * `<path>-journal` — rollback journal (only present
    ///     transiently or under non-WAL configurations; try-delete
    ///     is harmless if absent)
    ///
    /// The caller must hold no other handles to the Database. The
    /// `Arc<Mutex<Connection>>` we own is the only handle by design
    /// (see the struct docstring), so the lock acquired below is
    /// sufficient. If a future refactor introduces a second
    /// connection, this method must be revisited.
    pub fn rebuild_to_empty(&self, config: &DbConfig) -> Result<(), DbError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| DbError::Config(format!("connection mutex poisoned: {e}")))?;

        // Replace the existing connection with a temporary in-memory
        // one. Dropping the old `Connection` releases its file
        // handle, which is required on Windows before `remove_file`
        // can succeed.
        *guard = Connection::open_in_memory()?;

        // `:memory:` path → nothing on disk to delete, just open a
        // new in-memory connection in place.
        let is_memory = self.path == Path::new(":memory:");
        if !is_memory {
            // Delete the main DB file + every SQLite side-file that
            // can sit alongside it. `remove_file` is best-effort
            // for the side-files (NotFound is fine — they may not
            // exist), but a failure on the main file is fatal
            // because callers depend on a fresh state.
            let path = &self.path;
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            for suffix in ["-wal", "-shm", "-journal"] {
                let side = sibling_with_suffix(path, suffix);
                if side.exists() {
                    // Side-file delete failure is non-fatal — SQLite
                    // will overwrite stale WAL/SHM on first checkpoint
                    // of the new connection. Log via stderr (we don't
                    // have a tracing handle in core).
                    if let Err(e) = std::fs::remove_file(&side) {
                        eprintln!(
                            "rebuild_to_empty: failed to remove {} (continuing): {e}",
                            side.display(),
                        );
                    }
                }
            }
            // Open fresh at the same path. `Connection::open` creates
            // the file if absent.
            let new_conn = Connection::open(path)?;
            Self::apply_init_pragmas(&new_conn, config)?;
            *guard = new_conn;
        } else {
            // For in-memory, the temporary we already swapped in IS
            // the new connection. Apply pragmas to it.
            Self::apply_init_pragmas(&guard, config)?;
        }

        Ok(())
    }
}

/// Compute the path of an SQLite side-file (e.g. `db-wal`, `db-shm`,
/// `db-journal`) given the main DB path. SQLite appends the suffix to
/// the full filename rather than swapping the extension — `foo.db`'s
/// WAL is `foo.db-wal`, not `foo-wal`.
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_without_key_succeeds() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        db.with_conn(|c| {
            c.execute_batch("CREATE TABLE t (id INTEGER);")?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn foreign_keys_pragma_is_on() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        let val: i64 = db
            .with_conn(|c| {
                let v: i64 = c.query_row("PRAGMA foreign_keys;", [], |r| r.get(0))?;
                Ok(v)
            })
            .unwrap();
        assert_eq!(val, 1, "foreign_keys must be ON on every new connection");
    }

    #[cfg(feature = "sqlcipher")]
    #[test]
    fn file_backed_db_with_passphrase_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("execlaw.db");
        let cfg = DbConfig {
            path: path.clone(),
            key: Some(SqlCipherKey::Passphrase("hunter2-but-longer".into())),
        };
        let db = Database::open(&cfg).unwrap();
        db.with_conn(|c| {
            c.execute_batch(
                "CREATE TABLE sanity(id INTEGER PRIMARY KEY, label TEXT); \
                 INSERT INTO sanity(id, label) VALUES (1, 'ok');",
            )?;
            Ok(())
        })
        .unwrap();
        drop(db);

        // Re-open with the same key and read the row back.
        let db2 = Database::open(&cfg).unwrap();
        let label: String = db2
            .with_conn(|c| {
                let v: String =
                    c.query_row("SELECT label FROM sanity WHERE id = 1", [], |r| r.get(0))?;
                Ok(v)
            })
            .unwrap();
        assert_eq!(label, "ok");
    }

    #[cfg(feature = "sqlcipher")]
    #[test]
    fn wrong_passphrase_cannot_read_encrypted_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let good = DbConfig {
            path: path.clone(),
            key: Some(SqlCipherKey::Passphrase(
                "correct horse battery staple".into(),
            )),
        };
        let db = Database::open(&good).unwrap();
        db.with_conn(|c| {
            c.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (42);")?;
            Ok(())
        })
        .unwrap();
        drop(db);

        let bad = DbConfig {
            path,
            key: Some(SqlCipherKey::Passphrase("totally different".into())),
        };
        let db2 = Database::open(&bad).unwrap();
        // Reading the table should fail because SQLCipher can't decrypt it.
        let result: Result<i64, _> =
            db2.with_conn(|c| Ok(c.query_row("SELECT x FROM t", [], |r| r.get(0))?));
        assert!(
            result.is_err(),
            "wrong passphrase should NOT yield a readable DB"
        );
    }
}
