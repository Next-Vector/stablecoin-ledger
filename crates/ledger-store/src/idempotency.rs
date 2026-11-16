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

/// The resources a request produced, recorded with its key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    pub transaction: Option<TxId>,
    pub hold: Option<HoldId>,
    /// A registered script version: name and version.
    pub script: Option<(String, u32)>,
    pub webhook: Option<u64>,
}

/// What a write records next to its key (database ids).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Recorded<'a> {
    pub transaction: Option<i64>,
    pub hold: Option<i64>,
    pub script: Option<(&'a str, i32)>,
    pub webhook: Option<i64>,
}

/// What a key means for an incoming request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    /// Never used: run the request and record the key with it.
    New,
    /// Used before with the same request: return the resources it produced.
    Replay(Resources),
    /// Used before with a different request.
    Conflict,
}

impl PgStore {
    pub async fn key_status(&self, key: &IdempotencyKey) -> Result<KeyStatus, StoreError> {
        type Row = (
            Vec<u8>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<i32>,
            Option<i64>,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT request_hash, transaction_id, hold_id, script_name, script_version, webhook_id \
             FROM idempotency_keys WHERE key = $1",
        )
        .bind(&key.key)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        let Some((request_hash, transaction, hold, script_name, script_version, webhook)) = row
        else {
            return Ok(KeyStatus::New);
        };
        if request_hash != key.request_hash {
            return Ok(KeyStatus::Conflict);
        }
        let script = match (script_name, script_version) {
            (Some(name), Some(version)) => Some((name, u32::try_from(version).map_err(backend)?)),
            _ => None,
        };
        Ok(KeyStatus::Replay(Resources {
            transaction: transaction.map(tx_id).transpose()?,
            hold: hold
                .map(|id| u64::try_from(id).map(HoldId).map_err(backend))
                .transpose()?,
            script,
            webhook: webhook
                .map(|id| u64::try_from(id).map_err(backend))
                .transpose()?,
        }))
    }
}

/// Records `key` (if any) for the resources the current DB transaction wrote. If another request
/// recorded the same key meanwhile, fails with [`OpError::KeyReused`] so the caller's whole DB
/// transaction rolls back and the request can be answered from [`PgStore::key_status`].
pub(crate) async fn record<E>(
    db: &mut PgConnection,
    key: Option<&IdempotencyKey>,
    recorded: Recorded<'_>,
) -> Result<(), OpError<E>> {
    let Some(key) = key else {
        return Ok(());
    };
    let result = sqlx::query(
        "INSERT INTO idempotency_keys \
         (key, request_hash, transaction_id, hold_id, script_name, script_version, webhook_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&key.key)
    .bind(key.request_hash.as_slice())
    .bind(recorded.transaction)
    .bind(recorded.hold)
    .bind(recorded.script.map(|(name, _)| name))
    .bind(recorded.script.map(|(_, version)| version))
    .bind(recorded.webhook)
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
