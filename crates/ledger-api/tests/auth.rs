//! API key authentication and role checks (NFR-09) against the run-integration database.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use http_body_util::BodyExt;
use ledger_api::{AppState, app, protect};
use ledger_store::{Role, create_api_key, revoke_api_key};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

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

/// One dummy route per required role, each behind `protect`.
fn router(pool: &PgPool) -> Router {
    let state = AppState::new(pool.clone());
    let ok = || async { "ok" };
    Router::new()
        .merge(protect(
            Router::new().route("/read", get(ok)),
            &state,
            Role::Read,
        ))
        .merge(protect(
            Router::new().route("/write", get(ok)),
            &state,
            Role::Write,
        ))
        .merge(protect(
            Router::new().route("/admin", get(ok)),
            &state,
            Role::Admin,
        ))
        .with_state(state)
}

async fn call(
    router: Router,
    path: &str,
    authorization: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::get(path);
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    let response = router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

#[tokio::test]
async fn missing_or_invalid_credentials_are_unauthorized() {
    let pool = fresh_pool().await;
    let key = create_api_key(&pool, "test", Role::Admin).await.unwrap();
    let wrong_secret = format!("sl_{}_{}", key.key_id, "0".repeat(64));
    let unknown = format!("sl_ffffffff_{}", "0".repeat(64));

    for authorization in [
        None,
        Some("Basic abc"),
        Some("Bearer not-a-key"),
        Some(unknown.as_str()),
        Some(wrong_secret.as_str()),
    ] {
        let authorization = authorization.map(|value| {
            if value.starts_with("sl_") {
                bearer(value)
            } else {
                value.to_owned()
            }
        });
        let (status, headers, body) = call(router(&pool), "/read", authorization.as_deref()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
    }
}

#[tokio::test]
async fn revoked_key_is_unauthorized() {
    let pool = fresh_pool().await;
    let key = create_api_key(&pool, "test", Role::Admin).await.unwrap();
    assert_eq!(
        call(router(&pool), "/read", Some(&bearer(&key.plaintext)))
            .await
            .0,
        StatusCode::OK
    );
    assert!(revoke_api_key(&pool, &key.key_id).await.unwrap());
    assert!(
        !revoke_api_key(&pool, &key.key_id).await.unwrap(),
        "revoking twice reports nothing to do"
    );
    assert_eq!(
        call(router(&pool), "/read", Some(&bearer(&key.plaintext)))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn roles_are_enforced_hierarchically() {
    let pool = fresh_pool().await;
    let read = bearer(
        &create_api_key(&pool, "reader", Role::Read)
            .await
            .unwrap()
            .plaintext,
    );
    let write = bearer(
        &create_api_key(&pool, "writer", Role::Write)
            .await
            .unwrap()
            .plaintext,
    );
    let admin = bearer(
        &create_api_key(&pool, "admin", Role::Admin)
            .await
            .unwrap()
            .plaintext,
    );

    let expect = [
        (&read, "/read", StatusCode::OK),
        (&read, "/write", StatusCode::FORBIDDEN),
        (&read, "/admin", StatusCode::FORBIDDEN),
        (&write, "/read", StatusCode::OK),
        (&write, "/write", StatusCode::OK),
        (&write, "/admin", StatusCode::FORBIDDEN),
        (&admin, "/read", StatusCode::OK),
        (&admin, "/write", StatusCode::OK),
        (&admin, "/admin", StatusCode::OK),
    ];
    for (key, path, status) in expect {
        let (actual, _, body) = call(router(&pool), path, Some(key)).await;
        assert_eq!(actual, status, "{path}");
        if status == StatusCode::FORBIDDEN {
            assert_eq!(body["error"]["code"], "FORBIDDEN");
        }
    }
}

#[tokio::test]
async fn operational_routes_stay_public() {
    let pool = fresh_pool().await;
    for path in ["/health", "/health/ready", "/metrics", "/openapi.json"] {
        let (status, _, _) = call(app(AppState::new(pool.clone())), path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
}
