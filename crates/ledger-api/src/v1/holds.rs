//! Holds (FR-07): reserve funds, then capture them (fully or partially) or void them.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use ledger_core::{AccountAddress, AssetId, Hold, HoldError, HoldId, TxId};
use ledger_store::OpError;

use super::dto::{
    CaptureHoldRequest, CreateHoldRequest, HoldResponse, MetadataRequest, parse, parse_amount,
};
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody};
use crate::extract::{Mutation, PathParams};

/// Create a hold
///
/// Moves the amount from the account to the technical account `holds:{id}`.
#[utoipa::path(
    post, path = "/v1/holds", tag = "holds",
    request_body = CreateHoldRequest,
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    security(("api_key" = [])),
    responses(
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
        (status = 201, body = HoldResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS or ACCOUNT_FROZEN", body = ErrorBody),
    )
)]
pub async fn create_hold(
    State(state): State<AppState>,
    Mutation { body, key }: Mutation<CreateHoldRequest>,
) -> Result<Response, ApiError> {
    let operation = state.store.hold(
        parse::<AccountAddress>("account", &body.account)?,
        parse_amount("amount", &body.amount)?,
        parse::<AssetId>("asset", &body.asset)?,
        body.expires_at,
        body.metadata,
        key.as_ref(),
    );
    respond(&state, StatusCode::CREATED, key.as_ref(), operation).await
}

/// Capture a hold
///
/// Sends up to the held amount to the destination; the rest returns to the owner.
#[utoipa::path(
    post, path = "/v1/holds/{id}/capture", tag = "holds",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result"), ("id" = u64, Path, description = "Hold id")),
    request_body = CaptureHoldRequest,
    security(("api_key" = [])),
    responses(
        (status = 200, body = HoldResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 404, description = "HOLD_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT or HOLD_NOT_ACTIVE (captured, voided or expired)", body = ErrorBody),
        (status = 422, description = "HOLD_AMOUNT_EXCEEDED or ACCOUNT_FROZEN", body = ErrorBody),
    )
)]
pub async fn capture(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    Mutation { body, key }: Mutation<CaptureHoldRequest>,
) -> Result<Response, ApiError> {
    let operation = state.store.capture(
        HoldId(id),
        parse::<AccountAddress>("destination", &body.destination)?,
        parse_amount("amount", &body.amount)?,
        body.metadata,
        key.as_ref(),
    );
    respond(&state, StatusCode::OK, key.as_ref(), operation).await
}

/// Void a hold
///
/// Returns the whole held amount to the owner.
#[utoipa::path(
    post, path = "/v1/holds/{id}/void", tag = "holds",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result"), ("id" = u64, Path, description = "Hold id")),
    request_body(content = Option<MetadataRequest>, description = "Optional metadata"),
    security(("api_key" = [])),
    responses(
        (status = 200, body = HoldResponse),
        (status = 404, description = "HOLD_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT or HOLD_NOT_ACTIVE", body = ErrorBody),
    )
)]
pub async fn void(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    Mutation { body, key }: Mutation<MetadataRequest>,
) -> Result<Response, ApiError> {
    let operation = state.store.void(HoldId(id), body.metadata, key.as_ref());
    respond(&state, StatusCode::OK, key.as_ref(), operation).await
}

/// Runs a hold operation under its idempotency key; a replay shows the same hold, as it is now.
async fn respond(
    state: &AppState,
    status: StatusCode,
    key: Option<&ledger_store::IdempotencyKey>,
    operation: impl Future<Output = Result<(Hold, TxId), OpError<HoldError>>>,
) -> Result<Response, ApiError> {
    let (hold, tx, replayed) = match idempotency::run(&state.store, key, operation).await? {
        Outcome::Done((hold, tx)) => (hold, tx, false),
        Outcome::Replay(resources) => {
            let id = idempotency::recorded(resources.hold)?;
            let hold = idempotency::recorded(state.store.find_hold(id).await?)?;
            (hold, idempotency::recorded(resources.transaction)?, true)
        }
    };
    Ok(idempotency::respond(
        (status, Json(HoldResponse::new(&hold, tx))),
        replayed,
    ))
}
