//! Webhook delivery bookkeeping for the dispatcher (FR-53): claiming due deliveries without two
//! dispatchers ever taking the same one, and recording each attempt's outcome with exponential
//! backoff.

use std::time::Duration;

use ledger_core::StoreError;

use crate::{PgStore, backend};

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
}

type DeliveryRow = (String, i64, String, i32, String, String, i64, String);

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
                 SELECT d.event_id, d.endpoint_id FROM webhook_deliveries AS d \
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
             WHERE d.event_id = due.event_id AND d.endpoint_id = due.endpoint_id \
             RETURNING d.event_id, d.endpoint_id, e.url, d.attempts, ev.type, \
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
                |(event_id, endpoint_id, url, attempt, event_type, created_at, sequence, data)| {
                    Ok(Delivery {
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
    /// outcome that arrives after the lease ran out and the delivery was claimed again is ignored.
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
            "UPDATE webhook_deliveries \
             SET status = $4, \
                 next_attempt_at = CASE WHEN $4 = 'pending' THEN now() + make_interval(secs => $5) \
                                        ELSE next_attempt_at END, \
                 last_status_code = $6, last_error = $7, updated_at = now() \
             WHERE event_id = $1 AND endpoint_id = $2 AND attempts = $3 AND status = 'pending'",
        )
        .bind(&delivery.event_id)
        .bind(i64::try_from(delivery.endpoint_id).map_err(backend)?)
        .bind(i32::try_from(delivery.attempt).map_err(backend)?)
        .bind(status)
        .bind(retry_delay(delivery.attempt).as_secs_f64())
        .bind(outcome.status_code.map(i32::from))
        .bind(outcome.error.as_deref())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
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
}
