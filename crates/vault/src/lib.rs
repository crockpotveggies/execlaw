//! execlaw-vault
//!
//! Owns:
//!
//! - Loading the SQLCipher master key from the OS keyring (Linux Secret
//!   Service) or a passphrase-file fallback (§6.4).
//! - Admin-password hashing with Argon2id (§7.1).
//! - Opaque secret references (`secret://<plugin>/<name>`) — schema
//!   already in `execlaw-core`'s `vault_row` module; this crate wraps it
//!   with convenience + access-control helpers.

#![forbid(unsafe_code)]

pub mod keyring_key;
pub mod password;

pub use keyring_key::{load_or_create_master_key, KeyringLoadError};
pub use password::{hash_password, verify_password, PasswordError};
