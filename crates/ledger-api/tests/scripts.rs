//! Script endpoints over HTTP (FR-65, FR-66): transactions from inline or stored scripts,
//! dry-run, and the stored-script registry, against the run-integration database.

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

const CHECKOUT: &str = r#"vars {
  account $buyer
  account $merchant
  monetary $price
}

send $price (
  source = {
    @users:{$buyer}:bonus max [USDC@ethereum/6 5000000]
    @users:{$buyer}:wallet
  }
  destination = {
    1% to @platform:fees
    remaining to @merchants:{$merchant}:wallet
  }
)

set_tx_meta("order_id", "8812")
"#;

struct Api {
    router: Router,
    pool: PgPool,
    read: String,
    write: String,
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
        let read = create_api_key(&pool, "r", Role::Read)
            .await
            .unwrap()
            .plaintext;
        let write = create_api_key(&pool, "w", Role::Write)
            .await
            .unwrap()
            .plaintext;
        Self {
            router: app(AppState::new(pool.clone())),
            pool,
            read,
            write,
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
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, &self.write, Some(body)).await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, &self.read, None).await
    }

    async fn fund(&self, account: &str, amount: &str) {
        let (status, body) = self
            .post(
                "/v1/transactions",
                json!({"postings": [{"source": "world", "destination": account, "amount": amount, "asset": USDC}]}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    async fn transactions(&self) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM transactions")
            .fetch_one(&self.pool)
            .await
            .unwrap()
            .0
    }

    async fn balance(&self, address: &str) -> Value {
        let (_, body) = self.get(&format!("/v1/accounts/{address}")).await;
        body["balances"][USDC].clone()
    }
}

fn checkout_vars() -> Value {
    json!({"buyer": "42", "merchant": "7", "price": "USDC@ethereum/6 100000000"})
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
}

#[tokio::test]
async fn an_inline_script_becomes_one_audited_transaction() {
    let api = Api::new().await;
    api.fund("users:42:bonus", "3000000").await;
    api.fund("users:42:wallet", "200000000").await;

    let (status, tx) = api
        .post(
            "/v1/transactions",
            json!({"script": CHECKOUT, "vars": checkout_vars(), "metadata": {"channel": "web"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{tx}");
    let postings: Vec<(String, String, String)> = tx["postings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["source"].as_str().unwrap().to_owned(),
                p["destination"].as_str().unwrap().to_owned(),
                p["amount"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        postings,
        [
            (
                "users:42:bonus".into(),
                "platform:fees".into(),
                "1000000".into()
            ),
            (
                "users:42:bonus".into(),
                "merchants:7:wallet".into(),
                "2000000".into()
            ),
            (
                "users:42:wallet".into(),
                "merchants:7:wallet".into(),
                "97000000".into()
            ),
        ]
    );
    assert_eq!(tx["metadata"]["order_id"], "8812");
    assert_eq!(tx["metadata"]["channel"], "web");
    assert_eq!(tx["metadata"]["script.source"], CHECKOUT);
    let vars: Value =
        serde_json::from_str(tx["metadata"]["script.vars"].as_str().unwrap()).unwrap();
    assert_eq!(vars, checkout_vars());
    assert_eq!(api.balance("users:42:bonus").await, "0");
    assert_eq!(api.balance("merchants:7:wallet").await, "99000000");
}

#[tokio::test]
async fn dry_run_returns_postings_and_balances_without_writing() {
    let api = Api::new().await;
    api.fund("users:42:bonus", "3000000").await;
    api.fund("users:42:wallet", "200000000").await;
    let before = api.transactions().await;

    let (status, dry) = api
        .call(
            Method::POST,
            "/v1/scripts/dry-run",
            &api.read,
            Some(json!({"script": CHECKOUT, "vars": checkout_vars()})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    assert_eq!(dry["postings"].as_array().unwrap().len(), 3);
    assert_eq!(dry["metadata"]["order_id"], "8812");
    assert_eq!(dry["balances"]["users:42:bonus"][USDC], "0");
    assert_eq!(dry["balances"]["users:42:wallet"][USDC], "103000000");
    assert_eq!(dry["balances"]["merchants:7:wallet"][USDC], "99000000");
    assert_eq!(dry["balances"]["platform:fees"][USDC], "1000000");

    assert_eq!(api.transactions().await, before);
    assert_eq!(api.balance("users:42:wallet").await, "200000000");
    let (status, _) = api.get("/v1/accounts/merchants:7:wallet").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "dry-run must not create balance rows"
    );
}

#[tokio::test]
async fn dry_run_reports_what_the_ledger_would_reject() {
    let api = Api::new().await;
    api.fund("users:42:wallet", "1").await;
    let (status, body) = api
        .post(
            "/v1/scripts/dry-run",
            json!({"script": CHECKOUT, "vars": checkout_vars()}),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "INSUFFICIENT_FUNDS",
    );
    assert_eq!(body["error"]["details"]["needed"], "100000000");
}

#[tokio::test]
async fn stored_scripts_are_versioned_and_run_by_name() {
    let api = Api::new().await;
    api.fund("users:42:wallet", "200000000").await;

    let (status, v1) = api
        .post(
            "/v1/scripts",
            json!({"name": "checkout", "source": CHECKOUT}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v1}");
    assert_eq!(v1["name"], "checkout");
    assert_eq!(v1["version"], 1);
    let v2_source = CHECKOUT.replace("1% to", "2% to");
    let (_, v2) = api
        .post(
            "/v1/scripts",
            json!({"name": "checkout", "source": v2_source}),
        )
        .await;
    assert_eq!(v2["version"], 2);
    api.post("/v1/scripts", json!({"name": "refund", "source": "send [USDC@ethereum/6 1] (source = @world destination = @a)"}))
        .await;

    let (status, list) = api.get("/v1/scripts").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "checkout");
    assert_eq!(items[0]["version"], 2);
    assert!(items[0].get("source").is_none(), "{list}");

    let (_, page) = api.get("/v1/scripts?limit=1").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["next_cursor"], "checkout");
    let (_, rest) = api.get("/v1/scripts?limit=1&cursor=checkout").await;
    assert_eq!(rest["items"][0]["name"], "refund");
    assert_eq!(rest["next_cursor"], Value::Null);

    let (status, versions) = api.get("/v1/scripts/checkout/versions").await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions["items"][0]["version"], 1);
    assert_eq!(versions["items"][0]["source"], CHECKOUT);
    assert_eq!(versions["items"][1]["version"], 2);

    // By name runs the latest version (2% fee); a pinned version runs that one.
    let (status, latest) = api
        .post(
            "/v1/transactions",
            json!({"script_name": "checkout", "vars": checkout_vars()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{latest}");
    assert_eq!(latest["postings"][0]["amount"], "2000000");
    assert_eq!(latest["metadata"]["script.name"], "checkout");
    assert_eq!(latest["metadata"]["script.version"], "2");
    assert_eq!(latest["metadata"]["script.source"], v2_source);
    let (_, pinned) = api
        .post(
            "/v1/transactions",
            json!({"script_name": "checkout", "script_version": 1, "vars": checkout_vars()}),
        )
        .await;
    assert_eq!(pinned["postings"][0]["amount"], "1000000");

    let (status, body) = api.get("/v1/scripts/nope/versions").await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "SCRIPT_NOT_FOUND");
    let (status, body) = api
        .post(
            "/v1/transactions",
            json!({"script_name": "nope", "vars": {}}),
        )
        .await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "SCRIPT_NOT_FOUND");
    let (status, body) = api
        .post(
            "/v1/transactions",
            json!({"script_name": "checkout", "script_version": 9, "vars": checkout_vars()}),
        )
        .await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "SCRIPT_NOT_FOUND");
}

#[tokio::test]
async fn invalid_scripts_and_values_are_rejected_with_positions() {
    let api = Api::new().await;
    let (status, body) = api
        .post(
            "/v1/scripts",
            json!({"name": "broken", "source": "send (source = @world)"}),
        )
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "SCRIPT_INVALID");
    assert_eq!(body["error"]["details"]["line"], 1);
    assert_eq!(body["error"]["details"]["column"], 6);

    for name in ["", "Upper", "with space", &"n".repeat(65)] {
        let (status, body) = api
            .post("/v1/scripts", json!({"name": name, "source": "send [USDC@ethereum/6 1] (source = @world destination = @a)"}))
            .await;
        assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
    }

    let (status, body) = api
        .post("/v1/transactions", json!({"script": "send (", "vars": {}}))
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "SCRIPT_INVALID");

    let mut vars = checkout_vars();
    vars["price"] = json!("USDC@ethereum/6 1.5");
    let (status, body) = api
        .post(
            "/v1/transactions",
            json!({"script": CHECKOUT, "vars": vars}),
        )
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");

    let (status, body) = api
        .post(
            "/v1/transactions",
            json!({"script": "send [USDC@ethereum/6 10] (source = @world destination = { 50% to @a })"}),
        )
        .await;
    assert_error(
        status,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "SCRIPT_REJECTED",
    );

    for body in [
        json!({"vars": {}}),
        json!({"script": CHECKOUT, "script_name": "checkout"}),
        json!({"postings": [], "script": CHECKOUT}),
        json!({"postings": [{"source": "world", "destination": "a", "amount": "1", "asset": USDC}], "vars": {}}),
        json!({"script": CHECKOUT, "script_version": 1}),
    ] {
        let (status, response) = api.post("/v1/transactions", body.clone()).await;
        assert_error(
            status,
            &response,
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
        );
    }
    assert_eq!(api.transactions().await, 0);
}

#[tokio::test]
async fn script_transactions_and_registrations_honour_idempotency_keys() {
    let api = Api::new().await;
    api.fund("users:42:wallet", "200000000").await;
    let request = |path: &str, key: &str, body: &Value| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", api.write))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let send = |request: Request<Body>| {
        let router = api.router.clone();
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };

    let register = json!({"name": "checkout", "source": CHECKOUT});
    let (_, first) = send(request("/v1/scripts", "reg-1", &register)).await;
    let (status, retry) = send(request("/v1/scripts", "reg-1", &register)).await;
    assert_eq!(status, StatusCode::CREATED, "{retry}");
    assert_eq!(retry["version"], first["version"]);
    let (_, versions) = api.get("/v1/scripts/checkout/versions").await;
    assert_eq!(versions["items"].as_array().unwrap().len(), 1);

    let run = json!({"script_name": "checkout", "vars": checkout_vars()});
    let before = api.transactions().await;
    let (_, tx) = send(request("/v1/transactions", "pay-1", &run)).await;
    let (status, again) = send(request("/v1/transactions", "pay-1", &run)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(again["id"], tx["id"]);
    assert_eq!(api.transactions().await, before + 1);
}

#[tokio::test]
async fn script_routes_require_the_matching_role() {
    let api = Api::new().await;
    let (status, body) = api
        .call(
            Method::POST,
            "/v1/scripts",
            &api.read,
            Some(json!({"name": "x", "source": "send [USDC@ethereum/6 1] (source = @world destination = @a)"})),
        )
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");
    let (status, _) = api.get("/v1/scripts").await;
    assert_eq!(status, StatusCode::OK);
}
