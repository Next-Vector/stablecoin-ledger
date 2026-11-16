//! Holds (FR-07): reserve funds, then capture them (fully or partially) or void them.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use ledger_core::{AccountAddress, AssetId, HoldId};

use super::dto::{
    CaptureHoldRequest, CreateHoldRequest, HoldResponse, MetadataRequest, parse, parse_amount,
};
use crate::AppState;
use crate::error::{ApiError, ErrorBody};
use crate::extract::{JsonBody, PathParams};

/// Create a hold
///
/// Moves the amount from the account to the technical account `holds:{id}`.
#[utoipa::path(
    post, path = "/v1/holds", tag = "holds",
    request_body = CreateHoldRequest,
    security(("api_key" = [])),
    responses(
        (status = 201, body = HoldResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS or ACCOUNT_FROZEN", body = ErrorBody),
    )
)]
pub async fn create_hold(
    State(state): State<AppState>,
    JsonBody(request): JsonBody<CreateHoldRequest>,
) -> Result<(StatusCode, Json<HoldResponse>), ApiError> {
    let (hold, tx) = state
        .store
        .hold(
            parse::<AccountAddress>("account", &request.account)?,
            parse_amount("amount", &request.amount)?,
            parse::<AssetId>("asset", &request.asset)?,
            request.expires_at,
            request.metadata,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(HoldResponse::new(&hold, tx))))
}

/// Capture a hold
///
/// Sends up to the held amount to the destination; the rest returns to the owner.
#[utoipa::path(
    post, path = "/v1/holds/{id}/capture", tag = "holds",
    params(("id" = u64, Path, description = "Hold id")),
    request_body = CaptureHoldRequest,
    security(("api_key" = [])),
    responses(
        (status = 200, body = HoldResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 404, description = "HOLD_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "HOLD_NOT_ACTIVE (captured, voided or expired)", body = ErrorBody),
        (status = 422, description = "HOLD_AMOUNT_EXCEEDED or ACCOUNT_FROZEN", body = ErrorBody),
    )
)]
pub async fn capture(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    JsonBody(request): JsonBody<CaptureHoldRequest>,
) -> Result<Json<HoldResponse>, ApiError> {
    let (hold, tx) = state
        .store
        .capture(
            HoldId(id),
            parse::<AccountAddress>("destination", &request.destination)?,
            parse_amount("amount", &request.amount)?,
            request.metadata,
        )
        .await?;
    Ok(Json(HoldResponse::new(&hold, tx)))
}

/// Void a hold
///
/// Returns the whole held amount to the owner.
#[utoipa::path(
    post, path = "/v1/holds/{id}/void", tag = "holds",
    params(("id" = u64, Path, description = "Hold id")),
    request_body(content = Option<MetadataRequest>, description = "Optional metadata"),
    security(("api_key" = [])),
    responses(
        (status = 200, body = HoldResponse),
        (status = 404, description = "HOLD_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "HOLD_NOT_ACTIVE", body = ErrorBody),
    )
)]
pub async fn void(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    JsonBody(request): JsonBody<MetadataRequest>,
) -> Result<Json<HoldResponse>, ApiError> {
    let (hold, tx) = state.store.void(HoldId(id), request.metadata).await?;
    Ok(Json(HoldResponse::new(&hold, tx)))
}
