//! Webhook delivery bookkeeping (FR-53, FR-56): claiming due deliveries without two dispatchers
//! ever taking the same one, recording each attempt's outcome with exponential backoff, the
//! attempt log, and manual redelivery.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use ledger_core::StoreError;

use crate::idempotency::{self, IdempotencyKey, Recorded};
use crate::webhooks::WebhookError;
use crate::{OpError, PgStore, backend};

/// After this many failed attempts a delivery is marked `failed` (FR-53).
pub const MAX_ATTEMPTS: u32 = 15;
const FIRST_RETRY: Duration = Duration::from_secs(30);
const LAST_RETRY: Duration = Duration::from_secs(24 * 60 * 60);

/// The wait before the next attempt after failed attempt number `attempt` (1-based): 30 s,
/// doubling each time, capped at 24 h.
pub fn retry_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(31);
    FIRST_RETRY
        .checked_mul(1 << doublings)
        .map_or(LAST_RETRY, |delay| delay.min(LAST_RETRY))
}

/// A delivery claimed for one attempt, with what the dispatcher needs to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub id: u64,
    pub event_id: String,
    pub endpoint_id: u64,
    pub url: String,
    /// This attempt's number, from 1.
    pub attempt: u32,
    pub event_type: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub sequence: i64,
    /// The event payload as JSON text.
    pub data: String,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOutcome {
    pub succeeded: bool,
    pub status_code: Option<u16>,
    pub error: Option<String>,
    /// How long the attempt took.
    pub latency: Duration,
}

/// Where a delivery stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Pending,
    Succeeded,
    Failed,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown delivery status {0:?}")]
pub struct UnknownDeliveryStatus(String);

impl FromStr for DeliveryStatus {
    type Err = UnknownDeliveryStatus;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        [Self::Pending, Self::Succeeded, Self::Failed]
            .into_iter()
            .find(|status| status.as_str() == raw)
            .ok_or_else(|| UnknownDeliveryStatus(raw.to_owned()))
    }
}

/// One delivery of an event to a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDelivery {
    pub id: u64,
    pub endpoint_id: u64,
    pub status: DeliveryStatus,
    pub attempts: u32,
}

/// One entry of the attempt log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryAttempt {
    pub id: u64,
    pub delivery_id: u64,
    pub event_id: String,
    pub event_type: String,
    /// The attempt's number within its delivery, from 1.
    pub attempt: u32,
    pub succeeded: bool,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    pub error: Option<String>,
    /// Unix seconds when the attempt ended.
    pub attempted_at: i64,
}

type DeliveryRow = (i64, String, i64, String, i32, String, String, i64, String);
type AttemptRow = (
    i64,
    i64,
    String,
    String,
    i32,
    bool,
    Option<i32>,
    i64,
    Option<String>,
    i64,
);

impl PgStore {
    /// Claims up to `limit` due deliveries of enabled subscriptions for one attempt each. Rows
    /// locked by another dispatcher are skipped, and a claim pushes `next_attempt_at` forward by
    /// `lease`, so a delivery is retried only if this attempt is not recorded in time.
    pub async fn claim_deliveries(
        &self,
        limit: u32,
        lease: Duration,
    ) -> Result<Vec<Delivery>, StoreError> {
        let rows: Vec<DeliveryRow> = sqlx::query_as(
            "WITH due AS ( \
                 SELECT d.id, d.event_id, d.endpoint_id FROM webhook_deliveries AS d \
                 JOIN webhook_endpoints AS e ON e.id = d.endpoint_id \
                 WHERE d.status = 'pending' AND d.next_attempt_at <= now() AND e.enabled \
                 ORDER BY d.next_attempt_at LIMIT $1 \
                 FOR UPDATE OF d SKIP LOCKED \
             ) \
             UPDATE webhook_deliveries AS d \
             SET attempts = d.attempts + 1, \
                 next_attempt_at = now() + make_interval(secs => $2), \
                 updated_at = now() \
             FROM due \
             JOIN webhook_events AS ev ON ev.id = due.event_id \
             JOIN webhook_endpoints AS e ON e.id = due.endpoint_id \
             WHERE d.id = due.id \
             RETURNING d.id, d.event_id, d.endpoint_id, e.url, d.attempts, ev.type, \
                       to_char(ev.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                       ev.sequence, ev.data::text",
        )
        .bind(i64::from(limit))
        .bind(lease.as_secs_f64())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter()
            .map(
                |(
                    id,
                    event_id,
                    endpoint_id,
                    url,
                    attempt,
                    event_type,
                    created_at,
                    sequence,
                    data,
                )| {
                    Ok(Delivery {
                        id: u64::try_from(id).map_err(backend)?,
                        event_id,
                        endpoint_id: u64::try_from(endpoint_id).map_err(backend)?,
                        url,
                        attempt: u32::try_from(attempt).map_err(backend)?,
                        event_type,
                        created_at,
                        sequence,
                        data,
                    })
                },
            )
            .collect()
    }

    /// Records the outcome of `delivery`'s attempt: success ends it; a failure schedules the
    /// next attempt with [`retry_delay`], or marks it `failed` after [`MAX_ATTEMPTS`]. An
    /// outcome that arrives after the lease ran out and the delivery was claimed again does not
    /// change the delivery. Every outcome goes into the attempt log.
    pub async fn record_attempt(
        &self,
        delivery: &Delivery,
        outcome: &AttemptOutcome,
    ) -> Result<(), StoreError> {
        let status = if outcome.succeeded {
            "succeeded"
        } else if delivery.attempt >= MAX_ATTEMPTS {
            "failed"
        } else {
            "pending"
        };
        sqlx::query(
            "WITH updated AS ( \
                 UPDATE webhook_deliveries \
                 SET status = $4, \
                     next_attempt_at = CASE WHEN $4 = 'pending' \
                                            THEN now() + make_interval(secs => $5) \
                                            ELSE next_attempt_at END, \
                     last_status_code = $6, last_error = $7, updated_at = now() \
                 WHERE id = $1 AND attempts = $3 AND status = 'pending' \
             ) \
             INSERT INTO webhook_delivery_attempts \
                 (delivery_id, endpoint_id, attempt, succeeded, status_code, latency_ms, error) \
             VALUES ($1, $2, $3, $8, $6, $9, $7) \
             ON CONFLICT (delivery_id, attempt) DO NOTHING",
        )
        .bind(i64::try_from(delivery.id).map_err(backend)?)
        .bind(i64::try_from(delivery.endpoint_id).map_err(backend)?)
        .bind(i32::try_from(delivery.attempt).map_err(backend)?)
        .bind(status)
        .bind(retry_delay(delivery.attempt).as_secs_f64())
        .bind(outcome.status_code.map(i32::from))
        .bind(outcome.error.as_deref())
        .bind(outcome.succeeded)
        .bind(i64::try_from(outcome.latency.as_millis()).map_err(backend)?)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

impl PgStore {
    /// The attempt log of subscription `endpoint_id`, newest first: at most `limit` entries older
    /// than `before`, and the next cursor when more exist.
    pub async fn delivery_attempts(
        &self,
        endpoint_id: u64,
        before: Option<u64>,
        limit: u32,
    ) -> Result<(Vec<DeliveryAttempt>, Option<u64>), StoreError> {
        let mut rows: Vec<AttemptRow> = sqlx::query_as(
            "SELECT a.id, a.delivery_id, d.event_id, ev.type, a.attempt, a.succeeded, \
                    a.status_code, a.latency_ms, a.error, \
                    floor(extract(epoch FROM a.attempted_at))::bigint \
             FROM webhook_delivery_attempts AS a \
             JOIN webhook_deliveries AS d ON d.id = a.delivery_id \
             JOIN webhook_events AS ev ON ev.id = d.event_id \
             WHERE a.endpoint_id = $1 AND ($2::bigint IS NULL OR a.id < $2) \
             ORDER BY a.id DESC LIMIT $3",
        )
        .bind(i64::try_from(endpoint_id).map_err(backend)?)
        .bind(before.map(i64::try_from).transpose().map_err(backend)?)
        .bind(i64::from(limit) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let items = rows
            .into_iter()
            .map(attempt)
            .collect::<Result<Vec<_>, _>>()?;
        let next = more.then(|| items.last().map(|item| item.id)).flatten();
        Ok((items, next))
    }

    /// Every delivery of `event_id`, oldest first.
    pub async fn event_deliveries(&self, event_id: &str) -> Result<Vec<EventDelivery>, StoreError> {
        let rows: Vec<(i64, i64, String, i32)> = sqlx::query_as(
            "SELECT id, endpoint_id, status, attempts FROM webhook_deliveries \
             WHERE event_id = $1 ORDER BY id",
        )
        .bind(event_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter()
            .map(|(id, endpoint_id, status, attempts)| {
                Ok(EventDelivery {
                    id: u64::try_from(id).map_err(backend)?,
                    endpoint_id: u64::try_from(endpoint_id).map_err(backend)?,
                    status: status.parse().map_err(backend)?,
                    attempts: u32::try_from(attempts).map_err(backend)?,
                })
            })
            .collect()
    }

    /// Queues `event_id` again, with a fresh retry schedule, for every subscription it was
    /// delivered to that still exists. A subscription that already has a pending delivery of the
    /// event keeps just that one.
    pub async fn redeliver(
        &self,
        event_id: &str,
        key: Option<&IdempotencyKey>,
    ) -> Result<(), OpError<WebhookError>> {
        let mut db = self.pool.begin().await?;
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM webhook_events WHERE id = $1")
            .bind(event_id)
            .fetch_optional(&mut *db)
            .await?;
        if exists.is_none() {
            return Err(OpError::Rejected(WebhookError::EventNotFound(
                event_id.to_owned(),
            )));
        }
        sqlx::query(
            "INSERT INTO webhook_deliveries (event_id, endpoint_id) \
             SELECT DISTINCT event_id, endpoint_id FROM webhook_deliveries WHERE event_id = $1 \
             ON CONFLICT (event_id, endpoint_id) WHERE status = 'pending' DO NOTHING",
        )
        .bind(event_id)
        .execute(&mut *db)
        .await?;
        idempotency::record(&mut db, key, Recorded::default()).await?;
        db.commit().await?;
        Ok(())
    }
}

fn attempt(
    (
        id,
        delivery_id,
        event_id,
        event_type,
        attempt,
        succeeded,
        status_code,
        latency_ms,
        error,
        attempted_at,
    ): AttemptRow,
) -> Result<DeliveryAttempt, StoreError> {
    Ok(DeliveryAttempt {
        id: u64::try_from(id).map_err(backend)?,
        delivery_id: u64::try_from(delivery_id).map_err(backend)?,
        event_id,
        event_type,
        attempt: u32::try_from(attempt).map_err(backend)?,
        succeeded,
        status_code: status_code
            .map(u16::try_from)
            .transpose()
            .map_err(backend)?,
        latency_ms: u64::try_from(latency_ms).map_err(backend)?,
        error,
        attempted_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_from_30_seconds_doubling_to_a_24_hour_cap() {
        let secs = |attempt| retry_delay(attempt).as_secs();
        assert_eq!(secs(1), 30);
        assert_eq!(secs(2), 60);
        assert_eq!(secs(3), 120);
        assert_eq!(secs(12), 30 * 2048);
        assert_eq!(secs(13), 86_400);
        assert_eq!(secs(MAX_ATTEMPTS), 86_400);
        assert_eq!(secs(u32::MAX), 86_400);
    }

    #[test]
    fn delivery_statuses_round_trip_through_text() {
        for status in [
            DeliveryStatus::Pending,
            DeliveryStatus::Succeeded,
            DeliveryStatus::Failed,
        ] {
            assert_eq!(status.as_str().parse::<DeliveryStatus>().unwrap(), status);
        }
        assert!("queued".parse::<DeliveryStatus>().is_err());
    }
}
