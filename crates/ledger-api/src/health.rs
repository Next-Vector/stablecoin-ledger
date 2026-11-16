//! Liveness and readiness probes, outside the versioned API.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;

use crate::AppState;
use crate::error::{ApiError, ErrorBody};

/// A readiness probe must answer quickly, even when the database hangs.
const READINESS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthStatus {
    pub status: &'static str,
}

/// Liveness: the process is up and serving requests.
#[utoipa::path(get, path = "/health", tag = "health",
    responses((status = 200, description = "Service is alive", body = HealthStatus)))]
pub async fn live() -> Json<HealthStatus> {
    Json(HealthStatus { status: "ok" })
}

/// Readiness: the service can reach its database.
#[utoipa::path(get, path = "/health/ready", tag = "health",
    responses(
        (status = 200, description = "Ready to serve traffic", body = HealthStatus),
        (status = 503, description = "A dependency is unavailable", body = ErrorBody),
    ))]
pub async fn ready(State(state): State<AppState>) -> Result<Json<HealthStatus>, ApiError> {
    let ping = sqlx::query("SELECT 1").execute(&state.pool);
    match tokio::time::timeout(READINESS_TIMEOUT, ping).await {
        Ok(Ok(_)) => Ok(Json(HealthStatus { status: "ok" })),
        Ok(Err(error)) => {
            tracing::warn!(%error, "readiness: database query failed");
            Err(not_ready())
        }
        Err(_) => {
            tracing::warn!("readiness: database did not answer in time");
            Err(not_ready())
        }
    }
}

fn not_ready() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "NOT_READY",
        "the database is not reachable",
    )
}
