//! Readiness against the real run-integration database.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ledger_api::{AppState, app};
use sqlx::PgPool;
use tower::ServiceExt;

#[tokio::test]
async fn readiness_is_ok_when_the_database_answers() {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    let response = app(AppState::new(pool))
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
