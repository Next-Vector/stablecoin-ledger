//! End-to-end tests of the /v1 journal routes over HTTP (spec §8, FR-01–FR-08) against the
//! run-integration database. Requests go through the full router, API key auth included.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use ledger_api::{AppState, app};
use ledger_store::{Role, create_api_key};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const USDC: &str = "USDC@ethereum/6";

struct Api {
    router: Router,
    read: String,
    write: String,
    admin: String,
}

impl Api {
    async fn new() -> Self {
        let pool = fresh_pool().await;
        let key = |role| {
            let pool = pool.clone();
            async move { create_api_key(&pool, "test", role).await.unwrap().plaintext }
        };
        Self {
            read: key(Role::Read).await,
            write: key(Role::Write).await,
            admin: key(Role::Admin).await,
            router: app(AppState::new(pool, ledger_store::SecretBox::new([9; 32]))),
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {key}"));
        let body = match body {
            Some(value) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        self.send(request.body(body).unwrap()).await
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{status}: not JSON: {}", String::from_utf8_lossy(&bytes)));
        (status, body)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, &self.read, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, &self.write, Some(body)).await
    }

    /// Creates a transaction and asserts it succeeded; returns its id.
    async fn transfer(&self, postings: &[(&str, &str, &str)]) -> u64 {
        let (status, body) = self.post("/v1/transactions", transfer(postings)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_u64().unwrap()
    }

    async fn balance(&self, address: &str) -> Value {
        let (status, body) = self.get(&format!("/v1/accounts/{address}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["balances"][USDC].clone()
    }
}

async fn fresh_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();
    pool
}

fn transfer(postings: &[(&str, &str, &str)]) -> Value {
    let postings: Vec<Value> = postings
        .iter()
        .map(|&(source, destination, amount)| {
            json!({"source": source, "destination": destination, "amount": amount, "asset": USDC})
        })
        .collect();
    json!({ "postings": postings })
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

#[tokio::test]
async fn creates_reads_and_lists_transactions() {
    let api = Api::new().await;
    let (status, created) = api
        .post(
            "/v1/transactions",
            json!({
                "postings": [
                    {"source": "world", "destination": "users:42:wallet", "amount": "100000000", "asset": USDC},
                    {"source": "users:42:wallet", "destination": "platform:fees", "amount": "1000000", "asset": USDC}
                ],
                "metadata": {"order_id": "8812"}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["id"], 1);
    assert_eq!(created["postings"][0]["amount"], "100000000");
    assert_eq!(created["postings"][1]["destination"], "platform:fees");
    assert_eq!(created["metadata"]["order_id"], "8812");
    assert_eq!(created["reverts"], Value::Null);
    assert!(created["created_at"].as_i64().unwrap() > 0);

    let (status, fetched) = api.get("/v1/transactions/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    api.transfer(&[("world", "users:7:wallet", "5")]).await;
    let (status, page) = api
        .get("/v1/transactions?account=users:42:wallet&limit=10")
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], 1);
    assert_eq!(page["next_cursor"], Value::Null);

    let (_, first) = api.get("/v1/transactions?limit=1").await;
    assert_eq!(first["items"][0]["id"], 2);
    assert_eq!(first["next_cursor"], 2);
    let (_, second) = api.get("/v1/transactions?limit=1&cursor=2").await;
    assert_eq!(second["items"][0]["id"], 1);

    let (_, by_metadata) = api
        .get("/v1/transactions?metadata_key=order_id&metadata_value=8812")
        .await;
    assert_eq!(by_metadata["items"].as_array().unwrap().len(), 1);

    let (status, body) = api.get("/v1/transactions/99").await;
    assert_error(
        status,
        &body,
        StatusCode::NOT_FOUND,
        "TRANSACTION_NOT_FOUND",
    );
}

#[tokio::test]
async fn rejects_overdrafts_with_insufficient_funds() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;

    let (status, body) = api
        .post(
            "/v1/transactions",
            transfer(&[("users:1:wallet", "users:2:wallet", "101")]),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "INSUFFICIENT_FUNDS",
    );
    assert_eq!(body["error"]["details"]["account"], "users:1:wallet");
    assert_eq!(body["error"]["details"]["asset"], USDC);
    assert_eq!(api.balance("users:1:wallet").await, "100");
}

#[tokio::test]
async fn invalid_input_returns_validation_errors_in_the_envelope() {
    let api = Api::new().await;
    let posting = |amount: &str, destination: &str| json!({"postings": [{"source": "world", "destination": destination, "amount": amount, "asset": USDC}]});
    for body in [
        posting("1.5", "users:1:wallet"),
        posting("-1", "users:1:wallet"),
        posting("+1", "users:1:wallet"),
        posting("0", "users:1:wallet"),
        posting("", "users:1:wallet"),
        posting("340282366920938463463374607431768211456", "users:1:wallet"),
        posting("1", "Users:1"),
        posting("1", "world"),
        json!({"postings": []}),
        json!({"postings": [{"source": "world", "destination": "users:1:wallet", "amount": "1", "asset": "USDC"}]}),
        json!({"postings": [{"source": "world", "destination": "users:1:wallet", "amount": 1, "asset": USDC}]}),
        json!({"postings": [{"source": "world", "destination": "users:1:wallet", "amount": "1", "asset": USDC, "extra": true}]}),
        json!({"postings": [], "is_admin": true}),
        json!({}),
    ] {
        let (status, response) = api.post("/v1/transactions", body).await;
        assert_error(
            status,
            &response,
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
        );
    }

    let raw = Request::post("/v1/transactions")
        .header(header::AUTHORIZATION, format!("Bearer {}", api.write))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let (status, body) = api.send(raw).await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");

    for path in [
        "/v1/transactions/abc",
        "/v1/transactions?limit=0",
        "/v1/transactions?limit=501",
        "/v1/transactions?account=Not-An-Account",
        "/v1/transactions?metadata_key=order_id",
        "/v1/accounts/bad%20address",
        "/v1/balances?address=users:**",
        "/v1/balances",
    ] {
        let (status, body) = api.get(path).await;
        assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
    }
}

#[tokio::test]
async fn reverts_a_transaction_once() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;
    let spend = api
        .transfer(&[("users:1:wallet", "merchants:1:wallet", "30")])
        .await;

    let (status, reversal) = api
        .post(
            &format!("/v1/transactions/{spend}/revert"),
            json!({"metadata": {"reason": "refund"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{reversal}");
    assert_eq!(reversal["reverts"], spend);
    assert_eq!(reversal["metadata"]["reason"], "refund");
    assert_eq!(reversal["postings"][0]["source"], "merchants:1:wallet");
    assert_eq!(api.balance("users:1:wallet").await, "100");

    let (_, original) = api.get(&format!("/v1/transactions/{spend}")).await;
    assert_eq!(original["reverted_by"], reversal["id"]);

    let (status, body) = api
        .call(
            Method::POST,
            &format!("/v1/transactions/{spend}/revert"),
            &api.write,
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::CONFLICT, "ALREADY_REVERTED");

    let (status, body) = api.post("/v1/transactions/99/revert", json!({})).await;
    assert_error(
        status,
        &body,
        StatusCode::NOT_FOUND,
        "TRANSACTION_NOT_FOUND",
    );
}

#[tokio::test]
async fn shows_accounts_now_and_at_a_past_transaction() {
    let api = Api::new().await;
    let first = api.transfer(&[("world", "users:1:wallet", "100")]).await;
    api.transfer(&[("users:1:wallet", "users:2:wallet", "40")])
        .await;

    let (status, account) = api.get("/v1/accounts/users:1:wallet").await;
    assert_eq!(status, StatusCode::OK, "{account}");
    assert_eq!(account["address"], "users:1:wallet");
    assert_eq!(account["frozen"], false);
    assert_eq!(account["metadata"], json!({}));
    assert_eq!(account["balances"], json!({ USDC: "60" }));

    let (_, past) = api
        .get(&format!("/v1/accounts/users:1:wallet?at={first}"))
        .await;
    assert_eq!(past["balances"], json!({ USDC: "100" }));

    let (_, world) = api.get("/v1/accounts/world").await;
    assert_eq!(world["balances"], json!({ USDC: "-100" }));

    let (status, body) = api.get("/v1/accounts/users:404:wallet").await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND");
    let (status, body) = api.get("/v1/accounts/users:1:wallet?at=99").await;
    assert_error(
        status,
        &body,
        StatusCode::NOT_FOUND,
        "TRANSACTION_NOT_FOUND",
    );
}

#[tokio::test]
async fn aggregates_balances_by_address_pattern() {
    let api = Api::new().await;
    api.transfer(&[
        ("world", "users:1:wallet", "100"),
        ("world", "users:2:wallet", "5"),
        ("world", "users:2:pending", "9"),
    ])
    .await;

    let (status, body) = api.get("/v1/balances?address=users:*:wallet").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["address"], "users:*:wallet");
    assert_eq!(body["balances"], json!({ USDC: "105" }));

    let (_, filtered) = api
        .get("/v1/balances?address=users:*:wallet&asset=DAI@ethereum/18")
        .await;
    assert_eq!(filtered["balances"], json!({}));
}

#[tokio::test]
async fn holds_capture_partially_and_void() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;

    let (status, hold) = api
        .post(
            "/v1/holds",
            json!({"account": "users:1:wallet", "amount": "30", "asset": USDC, "metadata": {"order": "1"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{hold}");
    assert_eq!(hold["id"], 1);
    assert_eq!(hold["status"], "active");
    assert_eq!(hold["account"], "holds:1");
    assert_eq!(hold["amount"], "30");
    assert_eq!(hold["expires_at"], Value::Null);
    assert!(hold["transaction_id"].is_u64());
    assert_eq!(api.balance("users:1:wallet").await, "70");

    let (status, captured) = api
        .post(
            "/v1/holds/1/capture",
            json!({"destination": "merchants:1:wallet", "amount": "20"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{captured}");
    assert_eq!(captured["status"], "captured");
    assert_eq!(api.balance("merchants:1:wallet").await, "20");
    assert_eq!(api.balance("users:1:wallet").await, "80");

    let (status, body) = api
        .call(Method::POST, "/v1/holds/1/void", &api.write, None)
        .await;
    assert_error(status, &body, StatusCode::CONFLICT, "HOLD_NOT_ACTIVE");

    let (_, second) = api
        .post(
            "/v1/holds",
            json!({"account": "users:1:wallet", "amount": "50", "asset": USDC, "expires_at": 4102444800u64}),
        )
        .await;
    assert_eq!(second["expires_at"], 4102444800u64);
    let (status, body) = api
        .post(
            "/v1/holds/2/capture",
            json!({"destination": "merchants:1:wallet", "amount": "51"}),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "HOLD_AMOUNT_EXCEEDED",
    );
    let (status, voided) = api.post("/v1/holds/2/void", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{voided}");
    assert_eq!(voided["status"], "voided");
    assert_eq!(api.balance("users:1:wallet").await, "80");

    let (status, body) = api
        .post(
            "/v1/holds",
            json!({"account": "users:1:wallet", "amount": "81", "asset": USDC}),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "INSUFFICIENT_FUNDS",
    );
    let (status, body) = api.post("/v1/holds/99/void", json!({})).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "HOLD_NOT_FOUND");
}

#[tokio::test]
async fn a_capture_splits_between_destinations() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;
    let (status, hold) = api
        .post(
            "/v1/holds",
            json!({"account": "users:1:wallet", "amount": "30", "asset": USDC}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{hold}");

    // A withdrawal capture: the amount in flight and the fee, the rest back to the owner.
    let (status, captured) = api
        .post(
            "/v1/holds/1/capture",
            json!({"destinations": [
                {"destination": "chain:ethereum:in_flight", "amount": "25"},
                {"destination": "platform:fees", "amount": "1"},
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{captured}");
    assert_eq!(captured["status"], "captured");
    assert_eq!(api.balance("chain:ethereum:in_flight").await, "25");
    assert_eq!(api.balance("platform:fees").await, "1");
    assert_eq!(api.balance("users:1:wallet").await, "74");
    let (_, tx) = api
        .get(&format!("/v1/transactions/{}", captured["transaction_id"]))
        .await;
    assert_eq!(tx["postings"].as_array().unwrap().len(), 3, "{tx}");
}

#[tokio::test]
async fn split_captures_are_validated() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;
    api.post(
        "/v1/holds",
        json!({"account": "users:1:wallet", "amount": "30", "asset": USDC}),
    )
    .await;
    let part =
        |destination: &str, amount: &str| json!({"destination": destination, "amount": amount});

    let (status, body) = api
        .post(
            "/v1/holds/1/capture",
            json!({"destinations": [part("chain:ethereum:in_flight", "25"), part("platform:fees", "6")]}),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "HOLD_AMOUNT_EXCEEDED",
    );

    for (body, field) in [
        // Exactly one of the two forms.
        (
            json!({"destination": "merchants:1:wallet", "amount": "1", "destinations": [part("platform:fees", "1")]}),
            "destinations",
        ),
        (json!({}), "destinations"),
        (json!({"destination": "merchants:1:wallet"}), "amount"),
        (json!({"destinations": []}), "destinations"),
        (
            json!({"destinations": [part("platform:fees", "1"), part("not an account", "1")]}),
            "destinations[1].destination",
        ),
        (
            json!({"destinations": [part("platform:fees", "-1")]}),
            "destinations[0].amount",
        ),
    ] {
        let (status, response) = api.post("/v1/holds/1/capture", body.clone()).await;
        assert_error(
            status,
            &response,
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
        );
        assert_eq!(response["error"]["details"]["field"], field, "{body}");
    }
    // Nothing was captured.
    assert_eq!(api.balance("users:1:wallet").await, "70");
}

#[tokio::test]
async fn frozen_accounts_cannot_send_until_unfrozen() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;

    let (status, body) = api
        .call(
            Method::POST,
            "/v1/accounts/users:1:wallet/freeze",
            &api.write,
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");

    let (status, frozen) = api
        .call(
            Method::POST,
            "/v1/accounts/users:1:wallet/freeze",
            &api.admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{frozen}");
    assert_eq!(frozen["frozen"], true);

    let (status, body) = api
        .post(
            "/v1/transactions",
            transfer(&[("users:1:wallet", "users:2:wallet", "1")]),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "ACCOUNT_FROZEN",
    );
    assert_eq!(body["error"]["details"]["account"], "users:1:wallet");
    api.transfer(&[("world", "users:1:wallet", "1")]).await;

    let (status, unfrozen) = api
        .call(
            Method::POST,
            "/v1/accounts/users:1:wallet/unfreeze",
            &api.admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{unfrozen}");
    assert_eq!(unfrozen["frozen"], false);
    api.transfer(&[("users:1:wallet", "users:2:wallet", "1")])
        .await;
}

#[tokio::test]
async fn routes_require_the_matching_role() {
    let api = Api::new().await;
    api.transfer(&[("world", "users:1:wallet", "100")]).await;

    let (status, _) = api.get("/v1/transactions").await;
    assert_eq!(status, StatusCode::OK);
    for (method, path) in [
        (Method::POST, "/v1/transactions"),
        (Method::POST, "/v1/transactions/1/revert"),
        (Method::POST, "/v1/holds"),
        (Method::POST, "/v1/holds/1/capture"),
        (Method::POST, "/v1/holds/1/void"),
        (Method::POST, "/v1/accounts/users:1:wallet/freeze"),
        (Method::POST, "/v1/accounts/users:1:wallet/unfreeze"),
    ] {
        let (status, body) = api
            .call(method.clone(), path, &api.read, Some(json!({})))
            .await;
        assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");
    }

    let unauthenticated = Request::get("/v1/transactions")
        .body(Body::empty())
        .unwrap();
    let (status, body) = api.send(unauthenticated).await;
    assert_error(status, &body, StatusCode::UNAUTHORIZED, "UNAUTHORIZED");
}
