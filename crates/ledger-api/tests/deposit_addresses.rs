//! Deposit address registration over HTTP (FR-20): an address maps to exactly one account, an
//! account has one address per network, and only networks of registered assets are accepted.
//! Addresses are opaque strings — the ledger does not know any chain's format — against the
//! run-integration database.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use ledger_api::{AppState, app};
use ledger_store::{PgStore, Role, SecretBox, create_api_key};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const ADDRESS: &str = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed";
const OTHER: &str = "0xfb6916095ca1df60bb79ce92ce3ea74c37c5d359";
const TRON_ADDRESS: &str = "TQn9Y2khEsLJW1ChVWFMSMeRDow5KcbLSE";

struct Api {
    router: Router,
    write: String,
    read: String,
}

impl Api {
    /// A fresh database with USDC on `ethereum` and USDT on `tron` registered.
    async fn new() -> Self {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at the run-integration database");
        let pool = PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .execute(&pool)
            .await
            .unwrap();
        ledger_store::migrate(&pool).await.unwrap();
        let store = PgStore::new(pool.clone());
        for asset in ["USDC@ethereum/6", "USDT@tron/6"] {
            store.register_asset(&asset.parse().unwrap()).await.unwrap();
        }
        let write = create_api_key(&pool, "w", Role::Write)
            .await
            .unwrap()
            .plaintext;
        let read = create_api_key(&pool, "r", Role::Read)
            .await
            .unwrap()
            .plaintext;
        Self {
            router: app(AppState::new(pool, SecretBox::new([9; 32]))),
            write,
            read,
        }
    }

    async fn call(
        &self,
        key: &str,
        idempotency: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value, bool) {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/v1/deposit-addresses")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(idempotency) = idempotency {
            request = request.header("idempotency-key", idempotency);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
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

    async fn register(&self, network: &str, address: &str, account: &str) -> (StatusCode, Value) {
        let (status, body, _) = self
            .call(
                &self.write,
                None,
                json!({"network": network, "address": address, "account": account}),
            )
            .await;
        (status, body)
    }
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
}

#[tokio::test]
async fn an_address_is_registered_for_an_account() {
    let api = Api::new().await;

    let (status, body) = api.register("ethereum", ADDRESS, "users:42:wallet").await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["id"].as_u64().is_some(), "{body}");
    assert_eq!(body["network"], "ethereum");
    assert_eq!(body["address"], ADDRESS, "stored exactly as given");
    assert_eq!(body["account"], "users:42:wallet");
    assert!(body["created_at"].as_i64().is_some(), "{body}");
}

#[tokio::test]
async fn any_network_address_format_is_accepted() {
    let api = Api::new().await;

    let (status, body) = api.register("tron", TRON_ADDRESS, "users:42:wallet").await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["network"], "tron");
    assert_eq!(body["address"], TRON_ADDRESS);
    // One address per network: the same account can also have one on ethereum.
    let (status, body) = api.register("ethereum", ADDRESS, "users:42:wallet").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_registered_address_cannot_be_registered_again() {
    let api = Api::new().await;
    let (status, _) = api.register("ethereum", ADDRESS, "users:1:wallet").await;
    assert_eq!(status, StatusCode::CREATED);

    // Neither for another account nor for the same one.
    for account in ["users:2:wallet", "users:1:wallet"] {
        let (status, body) = api.register("ethereum", ADDRESS, account).await;
        assert_error(status, &body, StatusCode::CONFLICT, "DEPOSIT_ADDRESS_TAKEN");
    }
}

#[tokio::test]
async fn an_account_has_one_address_per_network() {
    let api = Api::new().await;
    api.register("ethereum", ADDRESS, "users:1:wallet").await;

    let (status, body) = api.register("ethereum", OTHER, "users:1:wallet").await;

    assert_error(
        status,
        &body,
        StatusCode::CONFLICT,
        "ACCOUNT_HAS_DEPOSIT_ADDRESS",
    );
}

#[tokio::test]
async fn only_networks_of_registered_assets_are_accepted() {
    let api = Api::new().await;

    let (status, body) = api.register("polygon", ADDRESS, "users:1:wallet").await;

    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "UNKNOWN_NETWORK",
    );
}

#[tokio::test]
async fn malformed_input_is_rejected() {
    let api = Api::new().await;
    let too_long = "a".repeat(256);
    for (body, field) in [
        // An address is opaque, but not empty, not padded and of bounded length.
        (
            json!({"network": "ethereum", "address": "", "account": "users:1:wallet"}),
            "address",
        ),
        (
            json!({"network": "ethereum", "address": " 0xabc", "account": "users:1:wallet"}),
            "address",
        ),
        (
            json!({"network": "ethereum", "address": too_long, "account": "users:1:wallet"}),
            "address",
        ),
        // Deposits credit a wallet account.
        (
            json!({"network": "ethereum", "address": ADDRESS, "account": "users:1:pending"}),
            "account",
        ),
        (
            json!({"network": "ethereum", "address": ADDRESS, "account": "not an account"}),
            "account",
        ),
    ] {
        let (status, body, _) = api.call(&api.write, None, body).await;
        assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
        assert_eq!(body["error"]["details"]["field"], field, "{body}");
    }
}

#[tokio::test]
async fn registration_needs_the_write_role() {
    let api = Api::new().await;

    let (status, body, _) = api
        .call(
            &api.read,
            None,
            json!({"network": "ethereum", "address": ADDRESS, "account": "users:1:wallet"}),
        )
        .await;

    assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");
}

#[tokio::test]
async fn a_repeated_request_replays_the_registration() {
    let api = Api::new().await;
    let body = json!({"network": "ethereum", "address": ADDRESS, "account": "users:1:wallet"});

    let (status, first, replayed) = api.call(&api.write, Some("reg-1"), body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert!(!replayed);

    let (status, again, replayed) = api.call(&api.write, Some("reg-1"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert!(replayed);
    assert_eq!(again, first);
}
