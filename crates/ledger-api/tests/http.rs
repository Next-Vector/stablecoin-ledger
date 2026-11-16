//! HTTP skeleton tests driven through the router in-process (no server, no database needed).

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use ledger_api::{AppState, app};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

/// A router whose database is unreachable: fine for every route that does not touch it.
fn router() -> Router {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .unwrap();
    app(AppState::new(pool, ledger_store::SecretBox::new([9; 32])))
}

async fn send(
    router: Router,
    method: Method,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(body)))
}

#[tokio::test]
async fn unknown_route_returns_the_error_envelope() {
    let (status, _, body) = send(router(), Method::GET, "/no-such-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = json(&body);
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn wrong_method_returns_the_error_envelope() {
    let (status, _, body) = send(router(), Method::POST, "/health").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(json(&body)["error"]["code"], "METHOD_NOT_ALLOWED");
}

#[tokio::test]
async fn liveness_is_always_ok() {
    let (status, _, body) = send(router(), Method::GET, "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["status"], "ok");
}

#[tokio::test]
async fn readiness_fails_with_the_envelope_when_the_database_is_down() {
    let (status, _, body) = send(router(), Method::GET, "/health/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&body)["error"]["code"], "NOT_READY");
}

#[tokio::test]
async fn serves_the_openapi_31_document() {
    let (status, _, body) = send(router(), Method::GET, "/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    let spec = json(&body);
    assert!(
        spec["openapi"].as_str().unwrap().starts_with("3.1"),
        "{}",
        spec["openapi"]
    );
    assert!(spec["paths"]["/health"].is_object());
    assert!(spec["paths"]["/health/ready"].is_object());
    assert!(spec["components"]["schemas"]["ErrorBody"].is_object());
}

#[tokio::test]
async fn documents_the_v1_routes_behind_bearer_authentication() {
    let (_, _, body) = send(router(), Method::GET, "/openapi.json").await;
    let spec = json(&body);
    assert_eq!(
        spec["components"]["securitySchemes"]["api_key"]["scheme"],
        "bearer"
    );
    for path in [
        "/v1/transactions",
        "/v1/transactions/{id}",
        "/v1/transactions/{id}/revert",
        "/v1/accounts/{address}",
        "/v1/accounts/{address}/freeze",
        "/v1/accounts/{address}/unfreeze",
        "/v1/balances",
        "/v1/holds",
        "/v1/holds/{id}/capture",
        "/v1/holds/{id}/void",
        "/v1/scripts",
        "/v1/scripts/dry-run",
        "/v1/scripts/{name}/versions",
        "/v1/webhooks",
        "/v1/webhooks/{id}",
        "/v1/webhooks/{id}/rotate-secret",
        "/v1/webhooks/{id}/deliveries",
        "/v1/webhook-events/{id}/redeliver",
        "/v1/deposit-addresses",
    ] {
        let operations = spec["paths"][path]
            .as_object()
            .unwrap_or_else(|| panic!("{path}"));
        for (method, operation) in operations {
            assert_eq!(
                operation["security"][0]["api_key"],
                serde_json::json!([]),
                "{method} {path}"
            );
        }
    }
    assert!(spec["paths"]["/health"]["get"]["security"].is_null());
}

#[tokio::test]
async fn documents_the_idempotency_key_on_every_mutating_route() {
    let (_, _, body) = send(router(), Method::GET, "/openapi.json").await;
    let spec = json(&body);
    let mut mutating = 0;
    for (path, operations) in spec["paths"].as_object().unwrap() {
        for method in ["post", "patch", "delete"] {
            // A dry-run is a POST that writes nothing, so it needs no idempotency key.
            let Some(operation) = operations
                .get(method)
                .filter(|_| path != "/v1/scripts/dry-run")
            else {
                continue;
            };
            mutating += 1;
            let documented = operation["parameters"]
                .as_array()
                .is_some_and(|parameters| {
                    parameters
                        .iter()
                        .any(|p| p["name"] == "Idempotency-Key" && p["in"] == "header")
                });
            assert!(documented, "{method} {path}");
            assert!(operation["responses"]["409"].is_object(), "{method} {path}");
        }
    }
    assert_eq!(mutating, 14);
}

#[tokio::test]
async fn v1_routes_require_an_api_key_without_touching_the_database_first() {
    let (status, headers, body) = send(router(), Method::GET, "/v1/transactions").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Bearer");
    assert_eq!(json(&body)["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn serves_swagger_ui_at_docs() {
    let (status, _, body) = send(router(), Method::GET, "/docs/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body)
            .to_lowercase()
            .contains("swagger")
    );
}

#[tokio::test]
async fn exposes_http_metrics_labelled_by_route_template() {
    let router = router();
    send(router.clone(), Method::GET, "/health").await;
    send(router.clone(), Method::GET, "/no-such-route").await;
    let (status, _, body) = send(router, Method::GET, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("http_requests_total"), "{text}");
    assert!(text.contains("route=\"/health\""), "{text}");
    assert!(
        text.contains("route=\"unmatched\""),
        "unknown paths must not become labels: {text}"
    );
    assert!(!text.contains("no-such-route"), "{text}");
    assert!(text.contains("http_request_duration_seconds"), "{text}");
}

#[tokio::test]
async fn assigns_or_propagates_a_request_id() {
    let (_, headers, _) = send(router(), Method::GET, "/health").await;
    assert!(!headers["x-request-id"].is_empty());

    let response = router()
        .oneshot(
            Request::get("/health")
                .header("x-request-id", "abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["x-request-id"], "abc-123");
}
