//! Webhook subscriptions over HTTP (FR-50, FR-54): CRUD, secret rotation with a 24-hour window
//! for the previous secret, and secrets encrypted at rest, against the run-integration database.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use ledger_api::ssrf::UrlPolicy;
use ledger_api::{AppState, app};
use ledger_store::{PgStore, Role, SecretBox, create_api_key};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

struct Api {
    router: Router,
    pool: PgPool,
    admin: String,
    write: String,
}

fn secrets() -> SecretBox {
    SecretBox::new([9; 32])
}

impl Api {
    /// Receivers anywhere, as in development: these tests are about subscriptions, not SSRF.
    async fn new() -> Self {
        Self::with_policy(UrlPolicy::AllowPrivate).await
    }

    async fn with_policy(policy: UrlPolicy) -> Self {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at the run-integration database");
        let pool = PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .execute(&pool)
            .await
            .unwrap();
        ledger_store::migrate(&pool).await.unwrap();
        let admin = create_api_key(&pool, "a", Role::Admin)
            .await
            .unwrap()
            .plaintext;
        let write = create_api_key(&pool, "w", Role::Write)
            .await
            .unwrap()
            .plaintext;
        Self {
            router: app(AppState::new(pool.clone(), secrets()).with_url_policy(policy)),
            pool,
            admin,
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call(method, path, &self.admin, body).await
    }

    async fn create(&self, url: &str, events: &[&str]) -> Value {
        let (status, body) = self
            .admin(
                Method::POST,
                "/v1/webhooks",
                Some(json!({"url": url, "event_types": events})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }

    async fn signing_secrets(&self, id: u64) -> Vec<String> {
        PgStore::new(self.pool.clone())
            .signing_secrets(&secrets(), id)
            .await
            .unwrap()
    }
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
}

#[tokio::test]
async fn subscriptions_are_created_listed_updated_and_deleted() {
    let api = Api::new().await;
    let created = api
        .create(
            "https://merchant.example/hooks",
            &["transaction.created", "hold.expired"],
        )
        .await;
    assert_eq!(created["id"], 1);
    assert_eq!(created["url"], "https://merchant.example/hooks");
    assert_eq!(
        created["event_types"],
        json!(["transaction.created", "hold.expired"])
    );
    assert_eq!(created["enabled"], true);
    assert_eq!(created["previous_secret_expires_at"], Value::Null);
    let secret = created["secret"].as_str().unwrap();
    assert!(
        secret.starts_with("whsec_") && secret.len() == "whsec_".len() + 64,
        "{secret}"
    );

    let (status, list) = api.admin(Method::GET, "/v1/webhooks", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert!(
        list["items"][0].get("secret").is_none(),
        "secrets are shown only once: {list}"
    );

    let (status, updated) = api
        .admin(
            Method::PATCH,
            "/v1/webhooks/1",
            Some(json!({"enabled": false, "event_types": ["account.frozen"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["event_types"], json!(["account.frozen"]));
    assert_eq!(updated["url"], "https://merchant.example/hooks");
    assert!(updated.get("secret").is_none());

    api.create("https://other.example/a", &["transaction.reverted"])
        .await;
    let (_, page) = api.admin(Method::GET, "/v1/webhooks?limit=1", None).await;
    assert_eq!(page["items"][0]["id"], 1);
    assert_eq!(page["next_cursor"], 1);
    let (_, rest) = api
        .admin(Method::GET, "/v1/webhooks?limit=1&cursor=1", None)
        .await;
    assert_eq!(rest["items"][0]["id"], 2);
    assert_eq!(rest["next_cursor"], Value::Null);

    let (status, _) = api.admin(Method::DELETE, "/v1/webhooks/1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = api.admin(Method::DELETE, "/v1/webhooks/1", None).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "WEBHOOK_NOT_FOUND");
    let (status, body) = api
        .admin(
            Method::PATCH,
            "/v1/webhooks/1",
            Some(json!({"enabled": true})),
        )
        .await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "WEBHOOK_NOT_FOUND");
    let (_, list) = api.admin(Method::GET, "/v1/webhooks", None).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn rotation_keeps_the_previous_secret_valid_for_24_hours() {
    let api = Api::new().await;
    let created = api
        .create("https://merchant.example/hooks", &["transaction.created"])
        .await;
    let first = created["secret"].as_str().unwrap().to_owned();
    assert_eq!(api.signing_secrets(1).await, std::slice::from_ref(&first));

    let (status, rotated) = api
        .admin(Method::POST, "/v1/webhooks/1/rotate-secret", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let second = rotated["secret"].as_str().unwrap().to_owned();
    assert_ne!(second, first);
    let window = rotated["previous_secret_expires_at"].as_i64().unwrap()
        - std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
    assert!((86_390..=86_400).contains(&window), "{window}");
    assert_eq!(
        api.signing_secrets(1).await,
        [second.clone(), first.clone()]
    );

    // Once the window has passed only the new secret signs.
    sqlx::query(
        "UPDATE webhook_endpoints SET previous_secret_expires_at = now() - interval '1 second'",
    )
    .execute(&api.pool)
    .await
    .unwrap();
    assert_eq!(api.signing_secrets(1).await, std::slice::from_ref(&second));

    // Rotating again retires the oldest secret: only the last one stays as previous.
    let (_, third) = api
        .admin(Method::POST, "/v1/webhooks/1/rotate-secret", None)
        .await;
    let third = third["secret"].as_str().unwrap().to_owned();
    assert_eq!(api.signing_secrets(1).await, [third, second]);

    let (status, body) = api
        .admin(Method::POST, "/v1/webhooks/9/rotate-secret", None)
        .await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "WEBHOOK_NOT_FOUND");
}

#[tokio::test]
async fn secrets_are_encrypted_at_rest() {
    let api = Api::new().await;
    let created = api
        .create("https://merchant.example/hooks", &["transaction.created"])
        .await;
    let secret = created["secret"].as_str().unwrap();
    let (stored,): (Vec<u8>,) = sqlx::query_as("SELECT secret FROM webhook_endpoints WHERE id = 1")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    let hex_secret = secret.trim_start_matches("whsec_");
    assert!(!String::from_utf8_lossy(&stored).contains(hex_secret));
    assert!(!hex::encode(&stored).contains(hex_secret));
    let other_key = PgStore::new(api.pool.clone())
        .signing_secrets(&SecretBox::new([1; 32]), 1)
        .await;
    assert!(
        other_key.is_err(),
        "a different key must not open the secret"
    );
}

#[tokio::test]
async fn invalid_subscriptions_are_rejected() {
    let api = Api::new().await;
    for body in [
        json!({"url": "ftp://merchant.example/hooks", "event_types": ["transaction.created"]}),
        json!({"url": "merchant.example/hooks", "event_types": ["transaction.created"]}),
        json!({"url": "https://", "event_types": ["transaction.created"]}),
        json!({"url": format!("https://merchant.example/{}", "a".repeat(2048)), "event_types": ["transaction.created"]}),
        json!({"url": "https://merchant.example/hooks", "event_types": []}),
        json!({"url": "https://merchant.example/hooks", "event_types": ["payment.created"]}),
        json!({"url": "https://merchant.example/hooks", "event_types": ["hold.expired", "hold.expired"]}),
        json!({"url": "https://merchant.example/hooks", "event_types": ["hold.expired"], "secret": "mine"}),
    ] {
        let (status, response) = api
            .admin(Method::POST, "/v1/webhooks", Some(body.clone()))
            .await;
        assert_error(
            status,
            &response,
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
        );
    }
    api.create("https://merchant.example/hooks", &["hold.expired"])
        .await;
    let (status, body) = api
        .admin(
            Method::PATCH,
            "/v1/webhooks/1",
            Some(json!({"event_types": []})),
        )
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
}

#[tokio::test]
async fn webhook_management_requires_the_admin_role() {
    let api = Api::new().await;
    for (method, path) in [
        (Method::GET, "/v1/webhooks"),
        (Method::POST, "/v1/webhooks"),
        (Method::PATCH, "/v1/webhooks/1"),
        (Method::DELETE, "/v1/webhooks/1"),
        (Method::POST, "/v1/webhooks/1/rotate-secret"),
    ] {
        let (status, body) = api
            .call(method.clone(), path, &api.write, Some(json!({})))
            .await;
        assert_error(status, &body, StatusCode::FORBIDDEN, "FORBIDDEN");
    }
}

#[tokio::test]
async fn creation_and_rotation_replay_under_an_idempotency_key() {
    let api = Api::new().await;
    let send = |path: &str, key: &str, body: Value| {
        let request = Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", api.admin))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from(body.to_string()))
            .unwrap();
        let router = api.router.clone();
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let body = json!({"url": "https://merchant.example/hooks", "event_types": ["hold.expired"]});
    let (_, first) = send("/v1/webhooks", "hook-1", body.clone()).await;
    let (status, retry) = send("/v1/webhooks", "hook-1", body).await;
    assert_eq!(status, StatusCode::CREATED, "{retry}");
    assert_eq!(retry["id"], first["id"]);
    assert_eq!(retry["secret"], first["secret"]);
    let (_, list) = api.admin(Method::GET, "/v1/webhooks", None).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);

    let (_, rotated) = send("/v1/webhooks/1/rotate-secret", "rot-1", json!({})).await;
    let (_, again) = send("/v1/webhooks/1/rotate-secret", "rot-1", json!({})).await;
    assert_eq!(again["secret"], rotated["secret"]);
    assert_eq!(
        api.signing_secrets(1).await.len(),
        2,
        "a replay must not rotate twice"
    );
}

#[tokio::test]
async fn in_production_urls_reaching_private_addresses_are_rejected() {
    let api = Api::with_policy(UrlPolicy::PublicOnly).await;
    for url in [
        "http://127.0.0.1/hooks",
        "http://localhost:8080/hooks",
        "http://2130706433/hooks",
        "http://10.1.2.3/hooks",
        "http://172.16.0.1/hooks",
        "http://192.168.1.1/hooks",
        "http://100.64.0.1/hooks",
        "http://169.254.169.254/latest/meta-data",
        "http://0.0.0.0/hooks",
        "http://[::1]/hooks",
        "http://[fe80::1]/hooks",
        "http://[fd00::1]/hooks",
        "http://[::ffff:127.0.0.1]/hooks",
        "https://receiver.invalid/hooks",
    ] {
        let (status, body) = api
            .admin(
                Method::POST,
                "/v1/webhooks",
                Some(json!({"url": url, "event_types": ["transaction.created"]})),
            )
            .await;
        assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
        assert_eq!(body["error"]["details"]["field"], "url", "{url}: {body}");
    }

    api.create("https://93.184.215.14/hooks", &["transaction.created"])
        .await;
    let (status, body) = api
        .admin(
            Method::PATCH,
            "/v1/webhooks/1",
            Some(json!({"url": "http://127.0.0.1:9000/hooks"})),
        )
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "VALIDATION_ERROR");
    let (_, listed) = api.admin(Method::GET, "/v1/webhooks", None).await;
    assert_eq!(listed["items"][0]["url"], "https://93.184.215.14/hooks");
}
