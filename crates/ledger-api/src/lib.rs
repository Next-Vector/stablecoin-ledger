//! REST API for the ledger: router, error envelope, health probes, metrics and OpenAPI docs.

mod auth;
pub mod config;
pub mod dispatcher;
mod error;
pub mod expiry;
mod extract;
mod health;
mod metrics;
pub mod ssrf;
mod v1;

use std::time::Duration;

use axum::Router;
use axum::http::{HeaderName, StatusCode};
use axum::routing::get;
use ledger_store::{PgStore, SecretBox};
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use ssrf::UrlPolicy;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
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
    store: PgStore,
    secrets: SecretBox,
    url_policy: UrlPolicy,
    metrics: PrometheusHandle,
}

impl AppState {
    /// `secrets` seals webhook signing secrets at rest. Webhook receivers must be public
    /// addresses unless [`AppState::with_url_policy`] says otherwise.
    pub fn new(pool: PgPool, secrets: SecretBox) -> Self {
        Self {
            store: PgStore::new(pool.clone()),
            secrets,
            url_policy: UrlPolicy::default(),
            pool,
            metrics: metrics::handle(),
        }
    }

    pub fn with_url_policy(mut self, url_policy: UrlPolicy) -> Self {
        self.url_policy = url_policy;
        self
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Stablecoin Ledger API",
        description = "Double-entry ledger for custodial stablecoin services."
    ),
    paths(
        health::live,
        health::ready,
        v1::transactions::create_transaction,
        v1::transactions::list_transactions,
        v1::transactions::get_transaction,
        v1::transactions::revert,
        v1::accounts::get_account,
        v1::accounts::aggregate_balances,
        v1::accounts::freeze,
        v1::accounts::unfreeze,
        v1::holds::create_hold,
        v1::holds::capture,
        v1::holds::void,
        v1::scripts::dry_run,
        v1::scripts::register_script,
        v1::scripts::list_scripts,
        v1::scripts::script_versions,
        v1::webhooks::create_webhook,
        v1::webhooks::list_webhooks,
        v1::webhooks::update_webhook,
        v1::webhooks::delete_webhook,
        v1::webhooks::rotate_secret,
        v1::webhooks::delivery_log,
        v1::webhooks::redeliver,
        v1::deposit_addresses::register_deposit_address,
    ),
    components(schemas(ErrorBody, health::HealthStatus)),
    modifiers(&BearerAuth)
)]
struct ApiDoc;

/// Declares the `Authorization: Bearer <API key>` scheme the /v1 operations require.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert_default()
            .add_security_scheme(
                v1::SECURITY_SCHEME,
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some("API key: `sl_<key_id>_<secret>`"))
                        .build(),
                ),
            );
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .merge(v1::routes(&state))
        .route("/health", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/metrics", get(metrics::render))
        // Swagger UI at the API root, the raw spec next to it.
        .merge(SwaggerUi::new("/api").url("/api-json", ApiDoc::openapi()))
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
