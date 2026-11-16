//! Webhook subscriptions (FR-50) and their signing secrets (FR-54). A secret is generated here,
//! returned once, and stored sealed with a [`SecretBox`]; rotation keeps the previous secret
//! signing for [`PREVIOUS_SECRET_TTL_HOURS`].

use std::fmt;
use std::str::FromStr;

use ledger_core::StoreError;

use crate::idempotency::{self, IdempotencyKey, Recorded};
use crate::{OpError, PgStore, SecretBox, backend};

/// How long a rotated-out secret keeps signing deliveries.
pub const PREVIOUS_SECRET_TTL_HOURS: u32 = 24;

/// Event types a subscription can receive (FR-51).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    TransactionCreated,
    TransactionReverted,
    DepositSeen,
    DepositConfirmed,
    DepositOrphaned,
    WithdrawalBroadcast,
    WithdrawalConfirmed,
    WithdrawalFailed,
    HoldExpired,
    AccountFrozen,
    AccountUnfrozen,
    ReconciliationMismatch,
}

impl EventType {
    pub const ALL: [EventType; 12] = [
        EventType::TransactionCreated,
        EventType::TransactionReverted,
        EventType::DepositSeen,
        EventType::DepositConfirmed,
        EventType::DepositOrphaned,
        EventType::WithdrawalBroadcast,
        EventType::WithdrawalConfirmed,
        EventType::WithdrawalFailed,
        EventType::HoldExpired,
        EventType::AccountFrozen,
        EventType::AccountUnfrozen,
        EventType::ReconciliationMismatch,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventType::TransactionCreated => "transaction.created",
            EventType::TransactionReverted => "transaction.reverted",
            EventType::DepositSeen => "deposit.seen",
            EventType::DepositConfirmed => "deposit.confirmed",
            EventType::DepositOrphaned => "deposit.orphaned",
            EventType::WithdrawalBroadcast => "withdrawal.broadcast",
            EventType::WithdrawalConfirmed => "withdrawal.confirmed",
            EventType::WithdrawalFailed => "withdrawal.failed",
            EventType::HoldExpired => "hold.expired",
            EventType::AccountFrozen => "account.frozen",
            EventType::AccountUnfrozen => "account.unfrozen",
            EventType::ReconciliationMismatch => "reconciliation.mismatch",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown event type {0:?}")]
pub struct UnknownEventType(pub String);

impl FromStr for EventType {
    type Err = UnknownEventType;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|event| event.as_str() == raw)
            .ok_or_else(|| UnknownEventType(raw.to_owned()))
    }
}

/// A subscription as the API shows it; never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEndpoint {
    pub id: u64,
    pub url: String,
    pub event_types: Vec<EventType>,
    pub enabled: bool,
    /// Unix seconds until which the previous secret still signs, after a rotation.
    pub previous_secret_expires_at: Option<i64>,
    pub created_at: i64,
}

/// Fields to change on a subscription; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookChanges {
    pub url: Option<String>,
    pub event_types: Option<Vec<EventType>>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("webhook {0} does not exist")]
    NotFound(u64),
}

type EndpointRow = (i64, String, Vec<String>, bool, Option<i64>, i64);
const ENDPOINT_COLUMNS: &str = "id, url, event_types, enabled, \
     floor(extract(epoch FROM previous_secret_expires_at))::bigint, \
     floor(extract(epoch FROM created_at))::bigint";

impl PgStore {
    /// Creates a subscription with a fresh secret; returns it with the secret (shown only now).
    pub async fn create_webhook(
        &self,
        secrets: &SecretBox,
        url: &str,
        event_types: &[EventType],
        enabled: bool,
        key: Option<&IdempotencyKey>,
    ) -> Result<(WebhookEndpoint, String), OpError<WebhookError>> {
        let secret = generate_secret().map_err(boxed)?;
        let sealed = secrets.seal(secret.as_bytes()).map_err(boxed)?;
        let mut db = self.pool.begin().await?;
        let row: EndpointRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "INSERT INTO webhook_endpoints (url, event_types, enabled, secret) \
             VALUES ($1, $2, $3, $4) RETURNING {ENDPOINT_COLUMNS}"
        )))
        .bind(url)
        .bind(names(event_types))
        .bind(enabled)
        .bind(&sealed)
        .fetch_one(&mut *db)
        .await?;
        record(&mut db, key, row.0).await?;
        db.commit().await?;
        Ok((endpoint(row).map_err(boxed)?, secret))
    }

    pub async fn webhook(&self, id: u64) -> Result<Option<WebhookEndpoint>, StoreError> {
        let row: Option<EndpointRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ENDPOINT_COLUMNS} FROM webhook_endpoints WHERE id = $1"
        )))
        .bind(db_id(id)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(endpoint).transpose()
    }

    /// Subscriptions by id, at most `limit` after `after`; the next cursor when more exist.
    pub async fn webhooks(
        &self,
        after: Option<u64>,
        limit: u32,
    ) -> Result<(Vec<WebhookEndpoint>, Option<u64>), StoreError> {
        let mut rows: Vec<EndpointRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ENDPOINT_COLUMNS} FROM webhook_endpoints \
             WHERE ($1::bigint IS NULL OR id > $1) ORDER BY id LIMIT $2"
        )))
        .bind(after.map(db_id).transpose()?)
        .bind(i64::from(limit) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let items = rows
            .into_iter()
            .map(endpoint)
            .collect::<Result<Vec<_>, _>>()?;
        let next = more.then(|| items.last().map(|item| item.id)).flatten();
        Ok((items, next))
    }

    pub async fn update_webhook(
        &self,
        id: u64,
        changes: &WebhookChanges,
        key: Option<&IdempotencyKey>,
    ) -> Result<WebhookEndpoint, OpError<WebhookError>> {
        let mut db = self.pool.begin().await?;
        let row: Option<EndpointRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE webhook_endpoints SET \
                 url = coalesce($2, url), \
                 event_types = coalesce($3, event_types), \
                 enabled = coalesce($4, enabled) \
             WHERE id = $1 RETURNING {ENDPOINT_COLUMNS}"
        )))
        .bind(db_id(id).map_err(boxed)?)
        .bind(changes.url.as_deref())
        .bind(changes.event_types.as_deref().map(names))
        .bind(changes.enabled)
        .fetch_optional(&mut *db)
        .await?;
        let row = row.ok_or(OpError::Rejected(WebhookError::NotFound(id)))?;
        record(&mut db, key, row.0).await?;
        db.commit().await?;
        endpoint(row).map_err(boxed)
    }

    pub async fn delete_webhook(
        &self,
        id: u64,
        key: Option<&IdempotencyKey>,
    ) -> Result<(), OpError<WebhookError>> {
        let mut db = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1")
            .bind(db_id(id).map_err(boxed)?)
            .execute(&mut *db)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(OpError::Rejected(WebhookError::NotFound(id)));
        }
        record(&mut db, key, db_id(id).map_err(boxed)?).await?;
        db.commit().await?;
        Ok(())
    }

    /// Replaces the secret; the old one keeps signing for [`PREVIOUS_SECRET_TTL_HOURS`] and an
    /// older previous secret is dropped. Returns the new secret (shown only now).
    pub async fn rotate_webhook_secret(
        &self,
        secrets: &SecretBox,
        id: u64,
        key: Option<&IdempotencyKey>,
    ) -> Result<(WebhookEndpoint, String), OpError<WebhookError>> {
        let secret = generate_secret().map_err(boxed)?;
        let sealed = secrets.seal(secret.as_bytes()).map_err(boxed)?;
        let mut db = self.pool.begin().await?;
        let row: Option<EndpointRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE webhook_endpoints SET \
                 previous_secret = secret, \
                 previous_secret_expires_at = now() + make_interval(hours => $3), \
                 secret = $2 \
             WHERE id = $1 RETURNING {ENDPOINT_COLUMNS}"
        )))
        .bind(db_id(id).map_err(boxed)?)
        .bind(&sealed)
        .bind(PREVIOUS_SECRET_TTL_HOURS as i32)
        .fetch_optional(&mut *db)
        .await?;
        let row = row.ok_or(OpError::Rejected(WebhookError::NotFound(id)))?;
        record(&mut db, key, row.0).await?;
        db.commit().await?;
        Ok((endpoint(row).map_err(boxed)?, secret))
    }

    /// The secrets that sign deliveries now: the current one first, then the previous one while
    /// its window lasts. Empty if the subscription does not exist.
    pub async fn signing_secrets(
        &self,
        secrets: &SecretBox,
        id: u64,
    ) -> Result<Vec<String>, StoreError> {
        let row: Option<(Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT secret, CASE WHEN previous_secret_expires_at > now() THEN previous_secret END \
             FROM webhook_endpoints WHERE id = $1",
        )
        .bind(db_id(id)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        let Some((current, previous)) = row else {
            return Ok(Vec::new());
        };
        [Some(current), previous]
            .into_iter()
            .flatten()
            .map(|sealed| {
                let plain = secrets.open(&sealed).map_err(backend)?;
                String::from_utf8(plain).map_err(backend)
            })
            .collect()
    }
}

/// `whsec_` and 32 random bytes in hex.
fn generate_secret() -> Result<String, StoreError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| backend(std::io::Error::other(error.to_string())))?;
    Ok(format!("whsec_{}", hex::encode(bytes)))
}

async fn record(
    db: &mut sqlx::PgConnection,
    key: Option<&IdempotencyKey>,
    webhook: i64,
) -> Result<(), OpError<WebhookError>> {
    idempotency::record(
        db,
        key,
        Recorded {
            webhook: Some(webhook),
            ..Recorded::default()
        },
    )
    .await
}

fn names(event_types: &[EventType]) -> Vec<String> {
    event_types
        .iter()
        .map(|event| event.as_str().to_owned())
        .collect()
}

fn db_id(id: u64) -> Result<i64, StoreError> {
    i64::try_from(id).map_err(backend)
}

fn boxed(error: impl std::error::Error + Send + Sync + 'static) -> OpError<WebhookError> {
    OpError::Backend(Box::new(error))
}

fn endpoint(
    (id, url, event_types, enabled, previous_secret_expires_at, created_at): EndpointRow,
) -> Result<WebhookEndpoint, StoreError> {
    Ok(WebhookEndpoint {
        id: u64::try_from(id).map_err(backend)?,
        url,
        event_types: event_types
            .iter()
            .map(|raw| raw.parse())
            .collect::<Result<_, _>>()
            .map_err(backend)?,
        enabled,
        previous_secret_expires_at,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_types_round_trip_through_text() {
        for event in EventType::ALL {
            assert_eq!(event.as_str().parse::<EventType>().unwrap(), event);
        }
        assert!("payment.created".parse::<EventType>().is_err());
    }

    #[test]
    fn generated_secrets_are_prefixed_and_unique() {
        let first = generate_secret().unwrap();
        let second = generate_secret().unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("whsec_"));
        assert_eq!(first.len(), "whsec_".len() + 64);
    }
}
