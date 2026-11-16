//! The webhook dispatcher against a local axum receiver (FR-53, FR-54, FR-55): signed delivery,
//! retries on schedule, giving up, timeouts, crash recovery and no double delivery between
//! dispatchers, with the run-integration database.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use hmac::{Hmac, KeyInit, Mac};
use ledger_api::dispatcher::{Dispatcher, DispatcherConfig};
use ledger_api::ssrf::UrlPolicy;
use ledger_core::{AccountAddress, AssetId, LedgerStore, Metadata, Posting, Transaction};
use ledger_store::{AttemptOutcome, EventType, PgStore, SecretBox};
use serde_json::Value;
use sha2::Sha256;
use sqlx::PgPool;

#[derive(Debug, Clone)]
struct Received {
    path: String,
    headers: HeaderMap,
    /// The body exactly as it arrived, which is what the signature covers.
    raw: Vec<u8>,
    body: Value,
}

#[derive(Clone, Default)]
struct Inbox {
    received: Arc<Mutex<Vec<Received>>>,
    responses: Arc<Mutex<VecDeque<StatusCode>>>,
    delay: Arc<Mutex<Duration>>,
}

impl Inbox {
    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn respond_with(&self, statuses: &[StatusCode]) {
        self.responses.lock().unwrap().extend(statuses);
    }
}

async fn receive(
    State(inbox): State<Inbox>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let delay = *inbox.delay.lock().unwrap();
    tokio::time::sleep(delay).await;
    inbox.received.lock().unwrap().push(Received {
        path,
        headers,
        raw: body.to_vec(),
        body: serde_json::from_slice(&body).unwrap(),
    });
    inbox
        .responses
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(StatusCode::OK)
}

/// A receiver on a free loopback port; returns its base URL.
async fn receiver() -> (String, Inbox) {
    let inbox = Inbox::default();
    let router = Router::new()
        .route("/{path}", post(receive))
        .with_state(inbox.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, inbox)
}

fn secrets() -> SecretBox {
    SecretBox::new([5; 32])
}

struct World {
    pool: PgPool,
    store: PgStore,
    url: String,
    inbox: Inbox,
}

impl World {
    async fn new() -> Self {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at the run-integration database");
        let pool = PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .execute(&pool)
            .await
            .unwrap();
        ledger_store::migrate(&pool).await.unwrap();
        let (url, inbox) = receiver().await;
        Self {
            store: PgStore::new(pool.clone()),
            pool,
            url,
            inbox,
        }
    }

    async fn subscribe(&self, path: &str, events: &[EventType], enabled: bool) -> String {
        let (_, secret) = self
            .store
            .create_webhook(
                &secrets(),
                &format!("{}/{path}", self.url),
                events,
                enabled,
                None,
            )
            .await
            .unwrap();
        secret
    }

    async fn deposit(&self, amount: u128) {
        let posting = Posting::new(
            AccountAddress::world(),
            "users:1:wallet".parse().unwrap(),
            amount,
            "USDC@ethereum/6".parse::<AssetId>().unwrap(),
        )
        .unwrap();
        self.store
            .apply(Transaction::new(vec![posting], Metadata::new()).unwrap())
            .await
            .unwrap();
    }

    fn dispatcher(&self) -> Dispatcher {
        Dispatcher::new(
            self.pool.clone(),
            secrets(),
            DispatcherConfig {
                timeout: Duration::from_millis(500),
                lease: Duration::from_secs(5),
                batch: 50,
                idle_wait: Duration::from_millis(10),
                // The receiver listens on loopback.
                url_policy: UrlPolicy::AllowPrivate,
            },
        )
        .unwrap()
    }

    /// (status, attempts, last status code, seconds until the next attempt) of the only delivery.
    async fn delivery(&self) -> (String, i32, Option<i32>, f64) {
        sqlx::query_as(
            "SELECT status, attempts, last_status_code, \
                    extract(epoch FROM next_attempt_at - now())::float8 \
             FROM webhook_deliveries",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn make_due(&self) {
        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now()")
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

/// Verifies `t=…,v1=…` against `secret` over the raw body as the receiver got it.
fn signed_by(received: &Received, secret: &str) -> bool {
    let headers = &received.headers;
    let header = headers["x-ledger-signature"].to_str().unwrap();
    let mut parts = header.split(',');
    let timestamp = parts.next().unwrap().strip_prefix("t=").unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        now.abs_diff(timestamp.parse().unwrap()) < 60,
        "stale timestamp {timestamp}"
    );
    let raw = &received.raw;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(raw);
    let expected = hex::encode(mac.finalize().into_bytes());
    parts.any(|part| part == format!("v1={expected}"))
}

#[tokio::test]
async fn delivers_a_signed_event_to_each_subscribed_endpoint_only() {
    let world = World::new().await;
    let secret = world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    world
        .subscribe("b", &[EventType::AccountFrozen], true)
        .await;
    world
        .subscribe("c", &[EventType::TransactionCreated], false)
        .await;
    world.deposit(100).await;

    let dispatcher = world.dispatcher();
    assert_eq!(dispatcher.run_once().await.unwrap(), 1);
    assert_eq!(dispatcher.run_once().await.unwrap(), 0);

    let received = world.inbox.received();
    assert_eq!(received.len(), 1, "{received:?}");
    let event = &received[0];
    assert_eq!(event.path, "a");
    assert_eq!(event.headers["content-type"], "application/json");
    assert_eq!(
        event.headers["x-ledger-event-id"],
        event.body["id"].as_str().unwrap()
    );
    assert!(signed_by(event, &secret));
    assert_eq!(event.body["type"], "transaction.created");
    assert_eq!(event.body["sequence"], 1);
    assert_eq!(event.body["data"]["transaction_id"], 1);
    assert_eq!(event.body["data"]["postings"][0]["amount"], "100");
    let created_at = event.body["created_at"].as_str().unwrap();
    assert!(
        created_at.ends_with('Z') && created_at.len() == 20,
        "{created_at}"
    );
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!(
        (status.as_str(), attempts, code),
        ("succeeded", 1, Some(200))
    );
}

#[tokio::test]
async fn during_a_rotation_both_secrets_sign() {
    let world = World::new().await;
    let old = world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    let (_, new) = world
        .store
        .rotate_webhook_secret(&secrets(), 1, None)
        .await
        .unwrap();
    world.deposit(1).await;
    world.dispatcher().run_once().await.unwrap();
    let event = &world.inbox.received()[0];
    assert!(signed_by(event, &new));
    assert!(signed_by(event, &old));
    assert!(!signed_by(event, "whsec_other"));
}

#[tokio::test]
async fn failed_attempts_are_retried_on_schedule_until_one_succeeds() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    world.inbox.respond_with(&[
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
    ]);
    world.deposit(1).await;
    let dispatcher = world.dispatcher();

    dispatcher.run_once().await.unwrap();
    let (status, attempts, code, wait) = world.delivery().await;
    assert_eq!((status.as_str(), attempts, code), ("pending", 1, Some(500)));
    assert!((29.0..=30.0).contains(&wait), "{wait}");
    assert_eq!(dispatcher.run_once().await.unwrap(), 0, "not due yet");

    world.make_due().await;
    dispatcher.run_once().await.unwrap();
    let (status, attempts, code, wait) = world.delivery().await;
    assert_eq!((status.as_str(), attempts, code), ("pending", 2, Some(503)));
    assert!((59.0..=60.0).contains(&wait), "{wait}");

    world.make_due().await;
    dispatcher.run_once().await.unwrap();
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!(
        (status.as_str(), attempts, code),
        ("succeeded", 3, Some(200))
    );
    let ids: Vec<Value> = world
        .inbox
        .received()
        .iter()
        .map(|r| r.body["id"].clone())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "the same event every time"
    );
}

#[tokio::test]
async fn a_delivery_is_marked_failed_after_15_attempts() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    world.deposit(1).await;
    sqlx::query("UPDATE webhook_deliveries SET attempts = 14")
        .execute(&world.pool)
        .await
        .unwrap();
    world
        .inbox
        .respond_with(&[StatusCode::INTERNAL_SERVER_ERROR]);
    let dispatcher = world.dispatcher();
    dispatcher.run_once().await.unwrap();
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!((status.as_str(), attempts, code), ("failed", 15, Some(500)));
    world.make_due().await;
    assert_eq!(dispatcher.run_once().await.unwrap(), 0);
}

#[tokio::test]
async fn timeouts_and_unreachable_receivers_count_as_failures() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    *world.inbox.delay.lock().unwrap() = Duration::from_secs(2);
    world.deposit(1).await;
    world.dispatcher().run_once().await.unwrap();
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!((status.as_str(), attempts, code), ("pending", 1, None));
    let (error,): (Option<String>,) = sqlx::query_as("SELECT last_error FROM webhook_deliveries")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert!(error.is_some_and(|e| !e.is_empty()));

    // A closed port: nothing listens there.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    sqlx::query("UPDATE webhook_endpoints SET url = $1")
        .bind(format!("http://127.0.0.1:{port}/a"))
        .execute(&world.pool)
        .await
        .unwrap();
    world.make_due().await;
    world.dispatcher().run_once().await.unwrap();
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!((status.as_str(), attempts, code), ("pending", 2, None));
}

#[tokio::test]
async fn concurrent_dispatchers_never_deliver_the_same_event_twice() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    for amount in 1..=60 {
        world.deposit(amount).await;
    }
    // Eight dispatchers taking three at a time keep claiming side by side.
    let dispatchers: Vec<Dispatcher> = (0..8)
        .map(|_| {
            Dispatcher::new(
                world.pool.clone(),
                secrets(),
                DispatcherConfig {
                    batch: 3,
                    url_policy: UrlPolicy::AllowPrivate,
                    ..DispatcherConfig::default()
                },
            )
            .unwrap()
        })
        .collect();
    loop {
        let rounds: Vec<_> = dispatchers
            .iter()
            .map(|dispatcher| {
                let dispatcher = dispatcher.clone();
                tokio::spawn(async move { dispatcher.run_once().await.unwrap() })
            })
            .collect();
        let mut attempted = 0;
        for round in rounds {
            attempted += round.await.unwrap();
        }
        if attempted == 0 {
            break;
        }
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for received in world.inbox.received() {
        *counts
            .entry(received.body["id"].as_str().unwrap().to_owned())
            .or_default() += 1;
    }
    assert_eq!(counts.len(), 60);
    assert!(counts.values().all(|&count| count == 1), "{counts:?}");
}

#[tokio::test]
async fn deliveries_locked_by_another_dispatcher_are_skipped_not_waited_for() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    world.deposit(1).await;
    world.deposit(2).await;

    // Another dispatcher holds one delivery mid-claim.
    let mut other = world.pool.begin().await.unwrap();
    let (held,): (String,) = sqlx::query_as(
        "SELECT event_id FROM webhook_deliveries ORDER BY event_id LIMIT 1 FOR UPDATE",
    )
    .fetch_one(&mut *other)
    .await
    .unwrap();

    let claimed = tokio::time::timeout(
        Duration::from_secs(2),
        world.store.claim_deliveries(10, Duration::from_secs(30)),
    )
    .await
    .expect("a locked delivery must be skipped, not waited for")
    .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_ne!(claimed[0].event_id, held);
    other.rollback().await.unwrap();
}

#[tokio::test]
async fn a_delivery_lost_with_a_crashed_dispatcher_is_retried_after_its_lease() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    world.deposit(1).await;

    // A dispatcher claims the delivery and dies before recording the attempt.
    let claimed = world
        .store
        .claim_deliveries(10, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let dispatcher = world.dispatcher();
    assert_eq!(dispatcher.run_once().await.unwrap(), 0, "still leased");

    // After the lease, a fresh dispatcher (the restarted process) delivers it.
    world.make_due().await;
    assert_eq!(dispatcher.run_once().await.unwrap(), 1);
    let (status, attempts, _, _) = world.delivery().await;
    assert_eq!((status.as_str(), attempts), ("succeeded", 2));

    // The dead dispatcher's late outcome changes nothing.
    world
        .store
        .record_attempt(
            &claimed[0],
            &AttemptOutcome {
                succeeded: false,
                status_code: Some(500),
                error: Some("late".to_owned()),
                latency: Duration::ZERO,
            },
        )
        .await
        .unwrap();
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!(
        (status.as_str(), attempts, code),
        ("succeeded", 2, Some(200))
    );
    assert_eq!(world.inbox.received().len(), 1);
}

#[tokio::test]
async fn only_subscriptions_present_when_the_event_happens_receive_it() {
    let world = World::new().await;
    world.deposit(1).await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    assert_eq!(world.dispatcher().run_once().await.unwrap(), 0);
    world.deposit(2).await;
    assert_eq!(world.dispatcher().run_once().await.unwrap(), 1);
}

#[tokio::test]
async fn in_production_private_receivers_are_never_contacted() {
    let world = World::new().await;
    world
        .subscribe("a", &[EventType::TransactionCreated], true)
        .await;
    world.deposit(1).await;
    let production = Dispatcher::new(
        world.pool.clone(),
        secrets(),
        DispatcherConfig {
            timeout: Duration::from_millis(500),
            ..DispatcherConfig::default()
        },
    )
    .unwrap();
    let last_error = || async {
        let (error,): (Option<String>,) =
            sqlx::query_as("SELECT last_error FROM webhook_deliveries")
                .fetch_one(&world.pool)
                .await
                .unwrap();
        error.unwrap_or_default()
    };

    // An address in the URL itself.
    production.run_once().await.unwrap();
    assert!(world.inbox.received().is_empty());
    let (status, attempts, code, _) = world.delivery().await;
    assert_eq!((status.as_str(), attempts, code), ("pending", 1, None));
    let error = last_error().await;
    assert!(error.contains("not allowed"), "{error}");

    // A name that resolves to loopback, which a check at subscription time alone would miss.
    let port = world.url.rsplit(':').next().unwrap();
    sqlx::query("UPDATE webhook_endpoints SET url = $1")
        .bind(format!("http://localhost:{port}/a"))
        .execute(&world.pool)
        .await
        .unwrap();
    world.make_due().await;
    production.run_once().await.unwrap();
    assert!(world.inbox.received().is_empty());
    let (_, attempts, code, _) = world.delivery().await;
    assert_eq!((attempts, code), (2, None));
    let error = last_error().await;
    assert!(error.contains("not allowed"), "{error}");

    // The same receiver is reachable when private addresses are allowed (development).
    world.make_due().await;
    world.dispatcher().run_once().await.unwrap();
    assert_eq!(world.inbox.received().len(), 1);
}
