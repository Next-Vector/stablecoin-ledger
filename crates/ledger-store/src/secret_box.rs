//! Encryption at rest for secrets the service must read back, such as webhook signing secrets
//! (FR-54 signs with the secret itself, so a hash would not do). AES-256-GCM with a random
//! 96-bit nonce per value, stored as `nonce || ciphertext`; the key comes from the environment.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

const NONCE_LEN: usize = 12;

/// Seals and opens secrets with one key. Its `Debug` output never shows the key.
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretBoxError {
    #[error("the key must be 64 hex characters (32 bytes)")]
    InvalidKey,
    #[error("the system random number generator failed")]
    Random,
    #[error("a sealed secret is corrupted or was sealed with another key")]
    Corrupted,
}

impl SecretBox {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(&key.into()),
        }
    }

    pub fn from_hex(key: &str) -> Result<Self, SecretBoxError> {
        let bytes: [u8; 32] = hex::decode(key)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(SecretBoxError::InvalidKey)?;
        Ok(Self::new(bytes))
    }

    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|_| SecretBoxError::Random)?;
        let ciphertext = self
            .cipher
            .encrypt(&Nonce::from(nonce), plaintext)
            .map_err(|_| SecretBoxError::Corrupted)?;
        Ok([nonce.as_slice(), &ciphertext].concat())
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        if sealed.len() < NONCE_LEN {
            return Err(SecretBoxError::Corrupted);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| SecretBoxError::Corrupted)?;
        self.cipher
            .decrypt(&Nonce::from(nonce), ciphertext)
            .map_err(|_| SecretBoxError::Corrupted)
    }
}

impl fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBox(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens_with_a_fresh_nonce_each_time() {
        let secrets = SecretBox::new([7; 32]);
        let first = secrets.seal(b"whsec_abc").unwrap();
        let second = secrets.seal(b"whsec_abc").unwrap();
        assert_ne!(first, second);
        assert_eq!(secrets.open(&first).unwrap(), b"whsec_abc");
        assert_eq!(secrets.open(&second).unwrap(), b"whsec_abc");
    }

    #[test]
    fn tampering_or_another_key_is_detected() {
        let secrets = SecretBox::new([7; 32]);
        let mut sealed = secrets.seal(b"whsec_abc").unwrap();
        assert_eq!(
            SecretBox::new([8; 32]).open(&sealed),
            Err(SecretBoxError::Corrupted)
        );
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert_eq!(secrets.open(&sealed), Err(SecretBoxError::Corrupted));
        assert_eq!(secrets.open(&[0; 5]), Err(SecretBoxError::Corrupted));
    }

    #[test]
    fn keys_are_64_hex_characters_and_never_printed() {
        assert!(SecretBox::from_hex(&"ab".repeat(32)).is_ok());
        for bad in ["", "ab", &"ab".repeat(33), &"zz".repeat(32)] {
            assert_eq!(
                SecretBox::from_hex(bad).unwrap_err(),
                SecretBoxError::InvalidKey
            );
        }
        let debug = format!("{:?}", SecretBox::from_hex(&"ab".repeat(32)).unwrap());
        assert!(!debug.contains("ab"), "{debug}");
    }
}
