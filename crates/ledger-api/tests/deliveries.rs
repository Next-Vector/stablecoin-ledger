//! The delivery log and manual redelivery over HTTP (FR-56), with the dispatcher sending to a
//! local axum receiver, against the run-integration database.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::post;
use http_body_util::BodyExt;
use ledger_api::dispatcher::{Dispatcher, DispatcherConfig};
use ledger_api::ssrf::UrlPolicy;
use ledger_api::{AppState, app};
use ledger_core::{AccountAddress, AssetId, LedgerStore, Metadata, Posting, Transaction};
use ledger_store::{EventType, PgStore, Role, SecretBox, create_api_key};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Inbox {
    /// Event ids in arrival order.
    received: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<VecDeque<StatusCode>>>,
}

async fn receive(State(inbox): State<Inbox>, body: Bytes) -> StatusCode {
    let body: Value = serde_json::from_slice(&body).unwrap();
    inbox
        .received
        .lock()
        .unwrap()
        .push(body["id"].as_str().unwrap().to_owned());
    inbox
        .responses
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(StatusCode::OK)
}

fn secrets() -> SecretBox {
    SecretBox::new([7; 32])
}

struct World {
    router: Router,
    pool: PgPool,
    store: PgStore,
    inbox: Inbox,
    admin: String,
    write: String,
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

        let inbox = Inbox::default();
        let receiver = Router::new()
            .route("/hook", post(receive))
            .with_state(inbox.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hook = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });

        let store = PgStore::new(pool.clone());
        store
            .create_webhook(
                &secrets(),
                &hook,
                &[EventType::TransactionCreated],
                true,
                None,
            )
            .await
            .unwrap();
        let admin = create_api_key(&pool, "a", Role::Admin)
            .await
            .unwrap()
            .plaintext;
        let write = create_api_key(&pool, "w", Role::Write)
            .await
            .unwrap()
            .plaintext;
        Self {
            router: app(AppState::new(pool.clone(), secrets())),
            pool,
            store,
            inbox,
            admin,
            write,
        }
    }

    fn dispatcher(&self) -> Dispatcher {
        Dispatcher::new(
            self.pool.clone(),
            secrets(),
            DispatcherConfig {
                timeout: Duration::from_millis(500),
                url_policy: UrlPolicy::AllowPrivate,
                ..DispatcherConfig::default()
            },
        )
        .unwrap()
    }

    /// A deposit; returns the id of its `transaction.created` event.
    async fn deposit(&self, amount: u128) -> String {
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
        let (id,): (String,) =
            sqlx::query_as("SELECT id FROM webhook_events ORDER BY created_at DESC LIMIT 1")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        id
    }

    fn respond_with(&self, statuses: &[StatusCode]) {
        self.inbox.responses.lock().unwrap().extend(statuses);
    }

    async fn make_due(&self) {
        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now()")
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        key: &str,
        idempotency_key: Option<&str>,
    ) -> (StatusCode, Value, bool) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {key}"));
        if let Some(idempotency_key) = idempotency_key {
            request = request.header("idempotency-key", idempotency_key);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let replayed = response.headers().contains_key("idempotent-replayed");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            replayed,
        )
    }

    async fn log(&self, query: &str) -> Value {
        let (status, body, _) = self
            .call(
                Method::GET,
                &format!("/v1/webhooks/1/deliveries{query}"),
                &self.admin,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn redeliver(&self, event: &str, idempotency_key: Option<&str>) -> (Value, bool) {
        let (status, body, replayed) = self
            .call(
                Method::POST,
                &format!("/v1/webhook-events/{event}/redeliver"),
                &self.admin,
                idempotency_key,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        (body, replayed)
    }

    /// (status, attempts) of every delivery of `event`, oldest first.
    async fn deliveries(&self, event: &str) -> Vec<(String, i32)> {
        sqlx::query_as(
            "SELECT status, attempts FROM webhook_deliveries WHERE event_id = $1 ORDER BY id",
        )
        .bind(event)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
}

#[tokio::test]
async fn the_log_records_every_attempt_newest_first() {
    let world = World::new().await;
    world.respond_with(&[StatusCode::INTERNAL_SERVER_ERROR]);
    let event = world.deposit(100).await;
    let dispatcher = world.dispatcher();
    dispatcher.run_once().await.unwrap();
    world.make_due().await;
    dispatcher.run_once().await.unwrap();

    let log = world.log("").await;
    let items = log["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{log}");
    assert_eq!(log["next_cursor"], Value::Null);

    let (second, first) = (&items[0], &items[1]);
    assert_eq!(second["event_id"], event.as_str());
    assert_eq!(second["event_type"], "transaction.created");
    assert_eq!(second["attempt"], 2);
    assert_eq!(second["succeeded"], true);
    assert_eq!(second["status_code"], 200);
    assert_eq!(second["error"], Value::Null);
    assert_eq!(first["attempt"], 1);
    assert_eq!(first["succeeded"], false);
    assert_eq!(first["status_code"], 500);
    assert_eq!(first["error"], "receiver answered 500");
    assert_eq!(first["delivery_id"], second["delivery_id"]);
    for item in items {
        assert!(item["latency_ms"].as_u64().is_some(), "{item}");
        assert!(item["attempted_at"].as_i64().unwrap() > 1_700_000_000);
    }
    assert!(second["id"].as_u64().unwrap() > first["id"].as_u64().unwrap());
}

#[tokio::test]
async fn the_log_records_errors_without_a_status_code_and_pages() {
    let world = World::new().await;
    world.deposit(1).await;
    world.deposit(2).await;
    world.deposit(3).await;
    sqlx::query("UPDATE webhook_endpoints SET url = 'http://127.0.0.1:1/closed'")
        .execute(&world.pool)
        .await
        .unwrap();
    world.dispatcher().run_once().await.unwrap();

    let page = world.log("?limit=2").await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{page}");
    assert_eq!(items[0]["status_code"], Value::Null);
    assert!(!items[0]["error"].as_str().unwrap().is_empty());
    let cursor = page["next_cursor"].as_u64().unwrap();
    let rest = world.log(&format!("?limit=2&cursor={cursor}")).await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1, "{rest}");
    assert_eq!(rest["next_cursor"], Value::Null);
    let ids: Vec<u64> = items
        .iter()
        .chain(rest["items"].as_array().unwrap())
        .map(|item| item["id"].as_u64().unwrap())
        .collect();
    assert!(ids.windows(2).all(|pair| pair[0] > pair[1]), "{ids:?}");
}

#[tokio::test]
async fn the_log_needs_an_existing_webhook_and_the_admin_role() {
    let world = World::new().await;
    let (status, body, _) = world
        .call(
            Method::GET,
            "/v1/webhooks/99/deliveries",
            &world.admin,
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "WEBHOOK_NOT_FOUND");
    let (status, body, _) = world
        .call(Method::GET, "/v1/webhooks/1/deliveries", &world.write, None)
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");
    let (status, body, _) = world
        .call(
            Method::GET,
            "/v1/webhooks/1/deliveries?limit=0",
            &world.admin,
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
}

#[tokio::test]
async fn a_delivered_event_is_sent_again_on_request() {
    let world = World::new().await;
    let event = world.deposit(1).await;
    let dispatcher = world.dispatcher();
    dispatcher.run_once().await.unwrap();

    let (body, replayed) = world.redeliver(&event, None).await;
    assert!(!replayed);
    assert_eq!(body["event_id"], event.as_str());
    let deliveries = body["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2, "{body}");
    assert_eq!(deliveries[0]["status"], "succeeded");
    assert_eq!(deliveries[1]["status"], "pending");
    assert_eq!(deliveries[1]["webhook_id"], 1);
    assert_eq!(deliveries[1]["attempts"], 0);

    assert_eq!(dispatcher.run_once().await.unwrap(), 1);
    let received = world.inbox.received.lock().unwrap().clone();
    assert_eq!(received, [event.clone(), event.clone()]);
    assert_eq!(
        world.deliveries(&event).await,
        [("succeeded".to_owned(), 1), ("succeeded".to_owned(), 1)]
    );
    let log = world.log("").await;
    assert_eq!(log["items"][0]["attempt"], 1);
    assert_ne!(
        log["items"][0]["delivery_id"],
        log["items"][1]["delivery_id"]
    );
}

#[tokio::test]
async fn a_failed_delivery_gets_a_fresh_retry_schedule() {
    let world = World::new().await;
    let event = world.deposit(1).await;
    sqlx::query("UPDATE webhook_deliveries SET status = 'failed', attempts = 15")
        .execute(&world.pool)
        .await
        .unwrap();
    world.redeliver(&event, None).await;
    world.respond_with(&[StatusCode::INTERNAL_SERVER_ERROR]);
    world.dispatcher().run_once().await.unwrap();
    let (wait,): (f64,) = sqlx::query_as(
        "SELECT extract(epoch FROM next_attempt_at - now())::float8 \
         FROM webhook_deliveries WHERE status = 'pending'",
    )
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!((29.0..=30.0).contains(&wait), "{wait}");
    assert_eq!(
        world.deliveries(&event).await,
        [("failed".to_owned(), 15), ("pending".to_owned(), 1)]
    );
}

#[tokio::test]
async fn redelivery_never_queues_a_second_pending_delivery() {
    let world = World::new().await;
    let event = world.deposit(1).await;
    let (body, _) = world.redeliver(&event, None).await;
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 1, "{body}");
    let redeliveries: Vec<_> = (0..5)
        .map(|_| {
            let router = world.router.clone();
            let admin = world.admin.clone();
            let path = format!("/v1/webhook-events/{event}/redeliver");
            tokio::spawn(async move {
                let request = Request::post(path)
                    .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                    .body(Body::empty())
                    .unwrap();
                router.oneshot(request).await.unwrap().status()
            })
        })
        .collect();
    for redelivery in redeliveries {
        assert_eq!(redelivery.await.unwrap(), StatusCode::ACCEPTED);
    }
    assert_eq!(world.deliveries(&event).await, [("pending".to_owned(), 0)]);
}

#[tokio::test]
async fn a_repeated_idempotency_key_queues_once() {
    let world = World::new().await;
    let event = world.deposit(1).await;
    world.dispatcher().run_once().await.unwrap();
    let (_, replayed) = world.redeliver(&event, Some("again-1")).await;
    assert!(!replayed);
    world.dispatcher().run_once().await.unwrap();
    let (body, replayed) = world.redeliver(&event, Some("again-1")).await;
    assert!(replayed);
    assert_eq!(body["event_id"], event.as_str());
    assert_eq!(world.deliveries(&event).await.len(), 2);
}

#[tokio::test]
async fn redelivering_an_unknown_event_is_not_found() {
    let world = World::new().await;
    let (status, body, _) = world
        .call(
            Method::POST,
            "/v1/webhook-events/evt_00000000000000000000000000000000/redeliver",
            &world.admin,
            None,
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::NOT_FOUND,
        "WEBHOOK_EVENT_NOT_FOUND",
    );
    let (status, body, _) = world
        .call(
            Method::POST,
            "/v1/webhook-events/evt_00000000000000000000000000000000/redeliver",
            &world.write,
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");
}
