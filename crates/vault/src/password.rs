//! Argon2id admin-password hashing (§7.1).

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PasswordError {
    #[error("argon2 error: {0}")]
    Argon2(String),
}

pub fn hash_password(plain: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::default();
    let hash = argon
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| PasswordError::Argon2(e.to_string()))?
        .to_string();
    Ok(hash)
}

pub fn verify_password(plain: &str, hash: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(hash).map_err(|e| PasswordError::Argon2(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_matches() {
        let h = hash_password("hunter2-longer").unwrap();
        assert!(verify_password("hunter2-longer", &h).unwrap());
        assert!(!verify_password("wrong", &h).unwrap());
    }

    #[test]
    fn hash_is_deterministic_only_under_same_salt() {
        // Salt is randomly generated per call; two hashes of the same
        // password should be different strings but both verify.
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b, "argon2id hashes must use a fresh salt per call");
        assert!(verify_password("same", &a).unwrap());
        assert!(verify_password("same", &b).unwrap());
    }
}
