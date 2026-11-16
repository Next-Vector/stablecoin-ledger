//! Idempotency-Key on every mutating /v1 route (FR-10, FR-11) over HTTP, against the
//! run-integration database.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use ledger_api::{AppState, app};
use ledger_store::{Role, create_api_key};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const USDC: &str = "USDC@ethereum/6";
const REPLAYED: &str = "idempotent-replayed";

struct Api {
    router: Router,
    pool: PgPool,
    admin: String,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn replayed(&self) -> bool {
        self.headers
            .get(REPLAYED)
            .is_some_and(|value| value == "true")
    }
}

impl Api {
    async fn new() -> Self {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at the run-integration database");
        let pool = PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .execute(&pool)
            .await
            .unwrap();
        ledger_store::migrate(&pool).await.unwrap();
        let admin = create_api_key(&pool, "test", Role::Admin)
            .await
            .unwrap()
            .plaintext;
        Self {
            router: app(AppState::new(pool.clone())),
            pool,
            admin,
        }
    }

    async fn raw(&self, method: Method, path: &str, key: Option<&str>, body: String) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.admin))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn post(&self, path: &str, key: Option<&str>, body: Value) -> Reply {
        self.raw(Method::POST, path, key, body.to_string()).await
    }

    async fn transactions(&self) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM transactions")
            .fetch_one(&self.pool)
            .await
            .unwrap()
            .0
    }

    async fn balance(&self, address: &str) -> Value {
        let reply = self
            .raw(
                Method::GET,
                &format!("/v1/accounts/{address}"),
                None,
                String::new(),
            )
            .await;
        reply.body["balances"][USDC].clone()
    }
}

fn deposit(account: &str, amount: &str) -> Value {
    json!({"postings": [{"source": "world", "destination": account, "amount": amount, "asset": USDC}]})
}

fn assert_conflict(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.body["error"]["code"], "IDEMPOTENCY_CONFLICT");
}

#[tokio::test]
async fn a_retried_transaction_returns_the_first_result_without_a_second_write() {
    let api = Api::new().await;
    let first = api
        .post(
            "/v1/transactions",
            Some("order-8812"),
            deposit("users:1:wallet", "100"),
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert!(!first.replayed());

    let retry = api
        .post(
            "/v1/transactions",
            Some("order-8812"),
            deposit("users:1:wallet", "100"),
        )
        .await;
    assert_eq!(retry.status, StatusCode::CREATED, "{}", retry.body);
    assert!(retry.replayed());
    assert_eq!(retry.body, first.body);
    assert_eq!(api.transactions().await, 1);
    assert_eq!(api.balance("users:1:wallet").await, "100");

    let without_key = api
        .post("/v1/transactions", None, deposit("users:1:wallet", "100"))
        .await;
    assert_eq!(without_key.status, StatusCode::CREATED);
    assert_eq!(api.transactions().await, 2);
}

#[tokio::test]
async fn the_same_body_with_other_formatting_is_the_same_request() {
    let api = Api::new().await;
    api.post(
        "/v1/transactions",
        Some("k1"),
        deposit("users:1:wallet", "100"),
    )
    .await;
    let reordered = format!(
        r#"{{ "postings" : [ {{"asset":"{USDC}","amount":"100","destination":"users:1:wallet","source":"world"}} ] }}"#
    );
    let retry = api
        .raw(Method::POST, "/v1/transactions", Some("k1"), reordered)
        .await;
    assert_eq!(retry.status, StatusCode::CREATED, "{}", retry.body);
    assert!(retry.replayed());
    assert_eq!(api.transactions().await, 1);
}

#[tokio::test]
async fn reusing_a_key_for_a_different_request_is_a_conflict() {
    let api = Api::new().await;
    api.post(
        "/v1/transactions",
        Some("k1"),
        deposit("users:1:wallet", "100"),
    )
    .await;

    assert_conflict(
        &api.post(
            "/v1/transactions",
            Some("k1"),
            deposit("users:1:wallet", "101"),
        )
        .await,
    );
    assert_conflict(
        &api.post(
            "/v1/holds",
            Some("k1"),
            json!({"account": "users:1:wallet", "amount": "1", "asset": USDC}),
        )
        .await,
    );
    assert_eq!(api.transactions().await, 1);
}

#[tokio::test]
async fn concurrent_requests_with_one_key_write_once() {
    let api = Api::new().await;
    let attempts: Vec<_> = (0..10)
        .map(|_| {
            let router = api.router.clone();
            let admin = api.admin.clone();
            tokio::spawn(async move {
                let request = Request::post("/v1/transactions")
                    .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "race")
                    .body(Body::from(deposit("users:1:wallet", "7").to_string()))
                    .unwrap();
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            })
        })
        .collect();
    let mut ids = Vec::new();
    for attempt in attempts {
        let (status, body) = attempt.await.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.push(body["id"].clone());
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert_eq!(api.transactions().await, 1);
    assert_eq!(api.balance("users:1:wallet").await, "7");
}

#[tokio::test]
async fn a_rejected_request_is_not_recorded_so_a_retry_is_evaluated_again() {
    let api = Api::new().await;
    let spend = json!({"postings": [{"source": "users:1:wallet", "destination": "users:2:wallet", "amount": "5", "asset": USDC}]});
    let rejected = api
        .post("/v1/transactions", Some("pay-1"), spend.clone())
        .await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);

    api.post("/v1/transactions", None, deposit("users:1:wallet", "5"))
        .await;
    let retry = api.post("/v1/transactions", Some("pay-1"), spend).await;
    assert_eq!(retry.status, StatusCode::CREATED, "{}", retry.body);
    assert!(!retry.replayed());
}

#[tokio::test]
async fn every_mutating_route_honours_the_key() {
    let api = Api::new().await;
    api.post("/v1/transactions", None, deposit("users:1:wallet", "100"))
        .await;
    let spend = api
        .post(
            "/v1/transactions",
            None,
            json!({"postings": [{"source": "users:1:wallet", "destination": "merchants:1:wallet", "amount": "10", "asset": USDC}]}),
        )
        .await;
    let spend_id = spend.body["id"].as_u64().unwrap();
    let hold = json!({"account": "users:1:wallet", "amount": "30", "asset": USDC});
    let capture = json!({"destination": "merchants:1:wallet", "amount": "20"});

    for (path, key, body, status) in [
        (
            format!("/v1/transactions/{spend_id}/revert"),
            "revert",
            json!({}),
            StatusCode::CREATED,
        ),
        (
            "/v1/holds".to_owned(),
            "hold-a",
            hold.clone(),
            StatusCode::CREATED,
        ),
        (
            "/v1/holds/1/capture".to_owned(),
            "capture",
            capture,
            StatusCode::OK,
        ),
        ("/v1/holds".to_owned(), "hold-b", hold, StatusCode::CREATED),
        (
            "/v1/holds/2/void".to_owned(),
            "void",
            json!({}),
            StatusCode::OK,
        ),
        (
            "/v1/accounts/users:1:wallet/freeze".to_owned(),
            "freeze",
            json!({}),
            StatusCode::OK,
        ),
    ] {
        let first = api.post(&path, Some(key), body.clone()).await;
        assert_eq!(first.status, status, "{path}: {}", first.body);
        let writes = api.transactions().await;
        let retry = api.post(&path, Some(key), body).await;
        assert_eq!(retry.status, status, "{path}: {}", retry.body);
        assert!(retry.replayed(), "{path}");
        assert_eq!(retry.body["id"], first.body["id"], "{path}");
        assert_eq!(api.transactions().await, writes, "{path}");
    }
    assert_eq!(api.balance("users:1:wallet").await, "80");
    assert_eq!(api.balance("merchants:1:wallet").await, "20");
}

#[tokio::test]
async fn malformed_keys_are_rejected() {
    let api = Api::new().await;
    for key in ["", &"k".repeat(256), "with space", "tab\there"] {
        let reply = api
            .post(
                "/v1/transactions",
                Some(key),
                deposit("users:1:wallet", "1"),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{key:?}: {}",
            reply.body
        );
        assert_eq!(reply.body["error"]["code"], "VALIDATION_ERROR");
    }
    assert_eq!(api.transactions().await, 0);
}
