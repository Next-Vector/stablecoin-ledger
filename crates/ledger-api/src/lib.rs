//! REST API for the ledger: router, error envelope, health probes, metrics and OpenAPI docs.

mod auth;
pub mod config;
mod error;
mod health;
mod metrics;

use std::time::Duration;

use axum::Router;
use axum::http::{HeaderName, StatusCode};
use axum::routing::get;
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub use auth::protect;
pub use error::{ApiError, ErrorBody};

/// OWASP API4: bounded request bodies and handler time.
const MAX_BODY_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    metrics: PrometheusHandle,
}

impl AppState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            metrics: metrics::handle(),
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Stablecoin Ledger API",
        description = "Double-entry ledger for custodial stablecoin services."
    ),
    paths(health::live, health::ready),
    components(schemas(ErrorBody, health::HealthStatus))
)]
struct ApiDoc;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/metrics", get(metrics::render))
        .merge(SwaggerUi::new("/docs").url("/openapi.json", ApiDoc::openapi()))
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async { ApiError::method_not_allowed() })
        .layer(axum::middleware::from_fn(metrics::track))
        .layer(TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<_>| {
            let request_id = request
                .headers()
                .get(&REQUEST_ID)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            tracing::info_span!("http", method = %request.method(), uri = %request.uri(), request_id)
        }))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, REQUEST_TIMEOUT))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(PropagateRequestIdLayer::new(REQUEST_ID))
        .layer(SetRequestIdLayer::new(REQUEST_ID, MakeRequestUuid))
        .with_state(state)
}
