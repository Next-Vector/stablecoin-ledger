//! Idempotency keys (FR-10, FR-11). Every mutating store operation takes an optional key and
//! records it inside its own DB transaction, next to the write it guards.

use ledger_core::{HoldId, StoreError, TxId};
use sqlx::PgConnection;

use crate::{OpError, PgStore, backend, tx_id};

/// A client-chosen key (1–255 visible ASCII characters) plus the hash of the request it was
/// first used with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyKey {
    key: String,
    request_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("an idempotency key must be 1 to {max} visible ASCII characters", max = IdempotencyKey::MAX_LEN)]
pub struct InvalidIdempotencyKey;

impl IdempotencyKey {
    pub const MAX_LEN: usize = 255;

    pub fn new(key: &str, request_hash: [u8; 32]) -> Result<Self, InvalidIdempotencyKey> {
        let valid =
            (1..=Self::MAX_LEN).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic());
        if !valid {
            return Err(InvalidIdempotencyKey);
        }
        Ok(Self {
            key: key.to_owned(),
            request_hash,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.key
    }
}

/// What a key means for an incoming request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatus {
    /// Never used: run the request and record the key with it.
    New,
    /// Used before with the same request: return the resources it produced.
    Replay {
        transaction: Option<TxId>,
        hold: Option<HoldId>,
    },
    /// Used before with a different request.
    Conflict,
}

impl PgStore {
    pub async fn key_status(&self, key: &IdempotencyKey) -> Result<KeyStatus, StoreError> {
        let row: Option<(Vec<u8>, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT request_hash, transaction_id, hold_id FROM idempotency_keys WHERE key = $1",
        )
        .bind(&key.key)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        let Some((request_hash, transaction, hold)) = row else {
            return Ok(KeyStatus::New);
        };
        if request_hash != key.request_hash {
            return Ok(KeyStatus::Conflict);
        }
        Ok(KeyStatus::Replay {
            transaction: transaction.map(tx_id).transpose()?,
            hold: hold
                .map(|id| u64::try_from(id).map(HoldId).map_err(backend))
                .transpose()?,
        })
    }
}

/// Records `key` (if any) for the resources the current DB transaction wrote. If another request
/// recorded the same key meanwhile, fails with [`OpError::KeyReused`] so the caller's whole DB
/// transaction rolls back and the request can be answered from [`PgStore::key_status`].
pub(crate) async fn record<E>(
    db: &mut PgConnection,
    key: Option<&IdempotencyKey>,
    transaction: Option<i64>,
    hold: Option<i64>,
) -> Result<(), OpError<E>> {
    let Some(key) = key else {
        return Ok(());
    };
    let result = sqlx::query(
        "INSERT INTO idempotency_keys (key, request_hash, transaction_id, hold_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&key.key)
    .bind(key.request_hash.as_slice())
    .bind(transaction)
    .bind(hold)
    .execute(db)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => Err(OpError::KeyReused),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_one_to_255_visible_ascii_characters() {
        let hash = [0u8; 32];
        for valid in ["a", "order-8812", "1:0xabc:7", &"k".repeat(255)] {
            assert!(IdempotencyKey::new(valid, hash).is_ok(), "{valid:?}");
        }
        for invalid in ["", &"k".repeat(256), "with space", "tab\there", "ключ"] {
            assert_eq!(
                IdempotencyKey::new(invalid, hash),
                Err(InvalidIdempotencyKey),
                "{invalid:?}"
            );
        }
    }
}
