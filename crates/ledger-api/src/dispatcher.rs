//! The webhook dispatcher (FR-53, FR-54, FR-55): delivers queued events to their subscriptions
//! with an HMAC signature, and lets the store schedule retries. Several dispatchers (replicas)
//! can run at once; the store never hands the same delivery to two of them.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use ledger_core::StoreError;
use ledger_store::{AttemptOutcome, Delivery, PgStore, SecretBox};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::ssrf::{self, PublicResolver, UrlPolicy};

pub const SIGNATURE_HEADER: &str = "x-ledger-signature";
pub const EVENT_ID_HEADER: &str = "x-ledger-event-id";

/// How a dispatcher works; [`DispatcherConfig::default`] follows the specification.
#[derive(Debug, Clone)]
pub struct DispatcherConfig {
    /// Per-request timeout (FR-53: 10 s).
    pub timeout: Duration,
    /// How long a claimed delivery stays reserved; longer than `timeout`, so an attempt in
    /// progress is not handed out again.
    pub lease: Duration,
    /// Deliveries claimed per round.
    pub batch: u32,
    /// The pause after a round that found nothing to deliver.
    pub idle_wait: Duration,
    /// Which receivers may be contacted (FR-57); public addresses only by default.
    pub url_policy: UrlPolicy,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            lease: Duration::from_secs(60),
            batch: 50,
            idle_wait: Duration::from_secs(1),
            url_policy: UrlPolicy::default(),
        }
    }
}

#[derive(Clone)]
pub struct Dispatcher {
    store: PgStore,
    secrets: SecretBox,
    client: reqwest::Client,
    config: DispatcherConfig,
}

impl Dispatcher {
    pub fn new(
        pool: PgPool,
        secrets: SecretBox,
        config: DispatcherConfig,
    ) -> Result<Self, reqwest::Error> {
        let builder = reqwest::Client::builder()
            .timeout(config.timeout)
            // A receiver answers itself; following redirects would send events elsewhere.
            .redirect(reqwest::redirect::Policy::none());
        let client = match config.url_policy {
            // Names resolve through the checking resolver, and no proxy may connect for us.
            UrlPolicy::PublicOnly => builder.dns_resolver(PublicResolver).no_proxy(),
            UrlPolicy::AllowPrivate => builder,
        }
        .build()?;
        Ok(Self {
            store: PgStore::new(pool),
            secrets,
            client,
            config,
        })
    }

    /// Claims due deliveries, sends them concurrently and records the outcomes. Returns how many
    /// attempts were made.
    pub async fn run_once(&self) -> Result<usize, StoreError> {
        let deliveries = self
            .store
            .claim_deliveries(self.config.batch, self.config.lease)
            .await?;
        let count = deliveries.len();
        let mut attempts = JoinSet::new();
        for delivery in deliveries {
            let dispatcher = self.clone();
            attempts.spawn(async move {
                let outcome = dispatcher.attempt(&delivery).await;
                dispatcher.store.record_attempt(&delivery, &outcome).await
            });
        }
        while let Some(recorded) = attempts.join_next().await {
            recorded.map_err(|error| StoreError::Backend(Box::new(error)))??;
        }
        Ok(count)
    }

    /// Delivers until `shutdown` turns true, pausing when there is nothing to do.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        while !*shutdown.borrow() {
            let wait = match self.run_once().await {
                Ok(0) => self.config.idle_wait,
                Ok(_) => Duration::ZERO,
                Err(error) => {
                    tracing::error!(%error, "webhook dispatch round failed");
                    self.config.idle_wait
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => {}
            }
        }
        tracing::info!("webhook dispatcher stopped");
    }

    async fn attempt(&self, delivery: &Delivery) -> AttemptOutcome {
        let started = Instant::now();
        let sent = self.send(delivery).await;
        let latency = started.elapsed();
        match sent {
            Ok(status) => {
                let succeeded = (200..300).contains(&status);
                tracing::info!(
                    event_id = %delivery.event_id,
                    endpoint_id = delivery.endpoint_id,
                    attempt = delivery.attempt,
                    status,
                    succeeded,
                    "webhook delivery attempted"
                );
                AttemptOutcome {
                    succeeded,
                    status_code: Some(status),
                    error: (!succeeded).then(|| format!("receiver answered {status}")),
                    latency,
                }
            }
            Err(error) => {
                tracing::warn!(
                    event_id = %delivery.event_id,
                    endpoint_id = delivery.endpoint_id,
                    attempt = delivery.attempt,
                    %error,
                    "webhook delivery failed"
                );
                AttemptOutcome {
                    succeeded: false,
                    status_code: None,
                    error: Some(error),
                    latency,
                }
            }
        }
    }

    /// POSTs the event; the receiver's status code, or why there was none.
    async fn send(&self, delivery: &Delivery) -> Result<u16, String> {
        let url = reqwest::Url::parse(&delivery.url).map_err(|error| error.to_string())?;
        if self.config.url_policy == UrlPolicy::PublicOnly {
            ssrf::check_literal(&url).map_err(|refused| refused.to_string())?;
        }
        let body = event_body(delivery)?;
        let secrets = self
            .store
            .signing_secrets(&self.secrets, delivery.endpoint_id)
            .await
            .map_err(|error| format!("signing secrets unavailable: {error}"))?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(EVENT_ID_HEADER, &delivery.event_id)
            .header(SIGNATURE_HEADER, signature(&secrets, timestamp, &body))
            .body(body)
            .send()
            .await
            .map_err(|error| describe(&error.without_url()))?;
        Ok(response.status().as_u16())
    }
}

/// An error with its causes, which carry the useful part (a refused address, a refused
/// connection) of a request error.
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The JSON a receiver gets (spec §5.6): id, type, created_at, sequence and data.
fn event_body(delivery: &Delivery) -> Result<Vec<u8>, String> {
    let data: Value = serde_json::from_str(&delivery.data).map_err(|error| error.to_string())?;
    serde_json::to_vec(&json!({
        "id": delivery.event_id,
        "type": delivery.event_type,
        "created_at": delivery.created_at,
        "sequence": delivery.sequence,
        "data": data,
    }))
    .map_err(|error| error.to_string())
}

/// `t=<unix>,v1=<hex>[,v1=<hex>…]`: an HMAC-SHA256 of `<t>.<body>` per signing secret, the
/// current one first, so receivers keep verifying during a rotation (FR-54).
pub fn signature(secrets: &[String], timestamp: u64, body: &[u8]) -> String {
    let mut header = format!("t={timestamp}");
    for secret in secrets {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        header.push_str(",v1=");
        header.push_str(&hex::encode(mac.finalize().into_bytes()));
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_timestamp_and_body_with_every_secret() {
        // Reference value from Python: hmac.new(b"whsec_a", b"1700000000.{}", sha256).hexdigest()
        let header = signature(&["whsec_a".to_owned()], 1_700_000_000, b"{}");
        assert_eq!(
            header,
            "t=1700000000,v1=04339dc4f01d6541e8d3b7e4bd93cd2d4f680ccb28592270900b08e49cc09e57"
        );

        let both = signature(
            &["whsec_new".to_owned(), "whsec_old".to_owned()],
            1_700_000_000,
            b"{}",
        );
        assert_eq!(both.matches(",v1=").count(), 2);
        assert!(both.starts_with("t=1700000000,v1="));
    }
}
