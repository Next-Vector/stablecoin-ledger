//! API keys (NFR-09): `sl_<key_id>_<secret>` where `key_id` is 8 hex chars (safe to log) and
//! `secret` is 32 random bytes in hex. Only a SHA-256 hash of the secret is stored: with 256 bits
//! of entropy a slow password hash adds latency to every request without adding security.

use std::str::FromStr;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;

/// Access levels, ordered: each role allows everything the lower ones do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    pub fn allows(self, required: Role) -> bool {
        self >= required
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }
}

impl FromStr for Role {
    type Err = ApiKeyError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "read" => Ok(Role::Read),
            "write" => Ok(Role::Write),
            "admin" => Ok(Role::Admin),
            other => Err(ApiKeyError::InvalidRole(other.to_owned())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    #[error("role {0:?} must be read, write or admin")]
    InvalidRole(String),
    #[error("key name must not be empty")]
    EmptyName,
    #[error("the system random number generator failed: {0}")]
    Random(getrandom::Error),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

const PREFIX: &str = "sl_";
const KEY_ID_HEX: usize = 8;
const SECRET_HEX: usize = 64;

/// A freshly generated key; `plaintext` is shown to the operator once and never stored.
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    pub key_id: String,
    pub plaintext: String,
    secret_hash: [u8; 32],
}

impl GeneratedKey {
    pub fn new() -> Result<Self, ApiKeyError> {
        let mut id = [0u8; KEY_ID_HEX / 2];
        let mut secret = [0u8; SECRET_HEX / 2];
        getrandom::fill(&mut id).map_err(ApiKeyError::Random)?;
        getrandom::fill(&mut secret).map_err(ApiKeyError::Random)?;
        let key_id = hex::encode(id);
        let secret = hex::encode(secret);
        Ok(Self {
            plaintext: format!("{PREFIX}{key_id}_{secret}"),
            secret_hash: hash(&secret),
            key_id,
        })
    }
}

/// A key as presented by a client, split into its parts.
#[derive(Debug)]
struct PresentedKey {
    key_id: String,
    secret: String,
}

impl PresentedKey {
    fn parse(raw: &str) -> Option<Self> {
        let (key_id, secret) = raw.strip_prefix(PREFIX)?.split_once('_')?;
        let lower_hex = |s: &str, len: usize| {
            s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        (lower_hex(key_id, KEY_ID_HEX) && lower_hex(secret, SECRET_HEX)).then(|| Self {
            key_id: key_id.to_owned(),
            secret: secret.to_owned(),
        })
    }

    fn secret_hash(&self) -> [u8; 32] {
        hash(&self.secret)
    }
}

fn hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Constant-time comparison, so response timing reveals nothing about the stored hash.
fn secrets_match(expected: &[u8], presented: &[u8]) -> bool {
    expected.ct_eq(presented).into()
}

/// The caller behind a valid key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    pub key_id: String,
    pub role: Role,
}

/// Why a presented key was refused; carries the `key_id` (never the secret) for audit logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    Malformed,
    UnknownKey(String),
    WrongSecret(String),
    Revoked(String),
}

/// Creates a key with `role` and stores its hash; returns the plaintext to hand out once.
pub async fn create_api_key(
    pool: &PgPool,
    name: &str,
    role: Role,
) -> Result<GeneratedKey, ApiKeyError> {
    if name.trim().is_empty() {
        return Err(ApiKeyError::EmptyName);
    }
    let key = GeneratedKey::new()?;
    sqlx::query("INSERT INTO api_keys (key_id, name, role, secret_hash) VALUES ($1, $2, $3, $4)")
        .bind(&key.key_id)
        .bind(name)
        .bind(role.as_str())
        .bind(key.secret_hash.as_slice())
        .execute(pool)
        .await?;
    Ok(key)
}

/// Revokes a key; returns false if it does not exist or was already revoked.
pub async fn revoke_api_key(pool: &PgPool, key_id: &str) -> Result<bool, ApiKeyError> {
    let result = sqlx::query(
        "UPDATE api_keys SET revoked_at = now() WHERE key_id = $1 AND revoked_at IS NULL",
    )
    .bind(key_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Checks a presented key. The outer error is a database failure; the inner result is the verdict.
pub async fn authenticate(
    pool: &PgPool,
    presented: &str,
) -> Result<Result<Authenticated, AuthFailure>, ApiKeyError> {
    let Some(key) = PresentedKey::parse(presented) else {
        return Ok(Err(AuthFailure::Malformed));
    };
    let row: Option<(String, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT role, secret_hash, revoked_at IS NOT NULL FROM api_keys WHERE key_id = $1",
    )
    .bind(&key.key_id)
    .fetch_optional(pool)
    .await?;
    let Some((role, stored_hash, revoked)) = row else {
        return Ok(Err(AuthFailure::UnknownKey(key.key_id)));
    };
    if !secrets_match(&stored_hash, &key.secret_hash()) {
        return Ok(Err(AuthFailure::WrongSecret(key.key_id)));
    }
    if revoked {
        return Ok(Err(AuthFailure::Revoked(key.key_id)));
    }
    Ok(Ok(Authenticated {
        key_id: key.key_id,
        role: role.parse()?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_form_a_hierarchy() {
        assert!(Role::Admin.allows(Role::Write));
        assert!(Role::Admin.allows(Role::Read));
        assert!(Role::Write.allows(Role::Read));
        assert!(Role::Write.allows(Role::Write));
        assert!(!Role::Write.allows(Role::Admin));
        assert!(!Role::Read.allows(Role::Write));
    }

    #[test]
    fn roles_round_trip_through_text() {
        for role in [Role::Read, Role::Write, Role::Admin] {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
        }
        assert!("root".parse::<Role>().is_err());
    }

    #[test]
    fn generated_keys_have_the_documented_shape_and_differ() {
        let first = GeneratedKey::new().unwrap();
        let second = GeneratedKey::new().unwrap();
        assert_ne!(first.plaintext, second.plaintext);
        assert_eq!(first.key_id.len(), 8);
        assert!(
            first
                .plaintext
                .starts_with(&format!("sl_{}_", first.key_id))
        );
        assert_eq!(first.plaintext.len(), "sl_".len() + 8 + 1 + 64);

        let parsed = PresentedKey::parse(&first.plaintext).unwrap();
        assert_eq!(parsed.key_id, first.key_id);
        assert_eq!(parsed.secret_hash(), first.secret_hash);
    }

    #[test]
    fn rejects_malformed_keys() {
        let valid_secret = "a".repeat(64);
        for raw in [
            "".to_owned(),
            "sl_".to_owned(),
            format!("xx_0123abcd_{valid_secret}"),
            format!("sl_0123abc_{valid_secret}"),
            format!("sl_0123abcz_{valid_secret}"),
            "sl_0123abcd_".to_owned() + &"a".repeat(63),
            "sl_0123abcd_".to_owned() + &"g".repeat(64),
            format!("sl_0123ABCD_{valid_secret}"),
        ] {
            assert!(PresentedKey::parse(&raw).is_none(), "{raw:?}");
        }
        assert!(PresentedKey::parse(&format!("sl_0123abcd_{valid_secret}")).is_some());
    }

    #[test]
    fn the_secret_hash_identifies_the_secret_only() {
        let a = PresentedKey::parse(&format!("sl_0123abcd_{}", "a".repeat(64))).unwrap();
        let b = PresentedKey::parse(&format!("sl_0123abcd_{}", "b".repeat(64))).unwrap();
        assert_ne!(a.secret_hash(), b.secret_hash());
        assert!(secrets_match(&a.secret_hash(), &a.secret_hash()));
        assert!(!secrets_match(&a.secret_hash(), &b.secret_hash()));
        assert!(!secrets_match(&a.secret_hash(), &[0u8; 3]));
    }
}
