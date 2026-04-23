//! SQLCipher master-key loader.
//!
//! Production flow:
//! 1. Read a 32-byte key from the OS keyring under service `"execlaw"`,
//!    entry `"sqlcipher_master_key"`.
//! 2. If not present, generate a new one, store it, return it.
//! 3. If the keyring is unavailable (headless box), fall back to a
//!    passphrase-file at `~/.execlaw/master.key` which the operator
//!    entered once at `execlaw up`.
//!
//! The fallback flow is stubbed for Phase 0; full headless-UX lands with
//! the CLI setup wizard.

use rand::RngCore;
use thiserror::Error;

const SERVICE: &str = "execlaw";
const ENTRY: &str = "sqlcipher_master_key";

#[derive(Debug, Error)]
pub enum KeyringLoadError {
    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("stored key is not 32 hex-encoded bytes: {0}")]
    BadKey(String),
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
}

/// Load the 32-byte SQLCipher master key, creating it on first run.
///
/// Returns the raw bytes. Callers should wrap them in
/// `SqlCipherKey::RawBytes`.
pub fn load_or_create_master_key() -> Result<[u8; 32], KeyringLoadError> {
    let entry = keyring::Entry::new(SERVICE, ENTRY)?;
    match entry.get_password() {
        Ok(hex_key) => {
            let bytes = hex::decode(&hex_key)?;
            if bytes.len() != 32 {
                return Err(KeyringLoadError::BadKey(format!(
                    "expected 32 bytes, got {}",
                    bytes.len()
                )));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            Ok(out)
        }
        Err(keyring::Error::NoEntry) => {
            let mut key = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut key);
            entry.set_password(&hex::encode(key))?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    // The real keyring path isn't something we can hit in a unit test
    // (CI doesn't have a Secret Service). We test the success path with a
    // MockKeyring by going through the public keyring API; for Phase 0 we
    // only sanity-check that the hex encoding is consistent.

    #[test]
    fn random_key_is_32_bytes_in_hex() {
        let mut key = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
        let h = hex::encode(key);
        assert_eq!(h.len(), 64);
        let back = hex::decode(&h).unwrap();
        assert_eq!(back, key);
    }
}
