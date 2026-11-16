//! Accounts: view with balances (FR-04), aggregated balances (FR-05), freeze (FR-08).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use ledger_core::{AccountAddress, AccountPattern, AssetId, TxId};
use ledger_store::IdempotencyKey;

use super::dto::EmptyRequest;
use super::dto::{AccountQuery, AccountResponse, BalancesQuery, BalancesResponse, balances, parse};
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody, transaction_not_found};
use crate::extract::{Mutation, PathParams, QueryParams};

/// Get an account
///
/// Metadata, flags and balances per asset — current, or as of a past transaction.
#[utoipa::path(
    get, path = "/v1/accounts/{address}", tag = "accounts",
    params(("address" = String, Path, example = "users:42:wallet"), AccountQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = AccountResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 404, description = "ACCOUNT_NOT_FOUND or TRANSACTION_NOT_FOUND (for `at`)", body = ErrorBody),
    )
)]
pub async fn get_account(
    State(state): State<AppState>,
    PathParams(address): PathParams<String>,
    QueryParams(query): QueryParams<AccountQuery>,
) -> Result<Json<AccountResponse>, ApiError> {
    let address = parse::<AccountAddress>("address", &address)?;
    let at = query.at.map(TxId);
    if let Some(at) = at
        && state.store.transaction(at).await?.is_none()
    {
        return Err(transaction_not_found());
    }
    account(&state, &address, at).await
}

/// Aggregated balances
///
/// Sums the current balances of all accounts matching an address pattern, per asset.
#[utoipa::path(
    get, path = "/v1/balances", tag = "accounts",
    params(BalancesQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = BalancesResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn aggregate_balances(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<BalancesQuery>,
) -> Result<Json<BalancesResponse>, ApiError> {
    let pattern = parse::<AccountPattern>("address", &query.address)?;
    let asset = query
        .asset
        .map(|raw| parse::<AssetId>("asset", &raw))
        .transpose()?;
    let totals = state
        .store
        .aggregate_balances(&pattern, asset.as_ref())
        .await?;
    Ok(Json(BalancesResponse {
        address: query.address,
        balances: balances(totals),
    }))
}

/// Freeze an account
///
/// A frozen account cannot be a posting source; it can still receive funds.
#[utoipa::path(
    post, path = "/v1/accounts/{address}/freeze", tag = "accounts",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result"), ("address" = String, Path, example = "users:42:wallet")),
    security(("api_key" = [])),
    responses(
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
        (status = 200, body = AccountResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn freeze(
    State(state): State<AppState>,
    PathParams(address): PathParams<String>,
    Mutation { key, .. }: Mutation<EmptyRequest>,
) -> Result<Response, ApiError> {
    set_frozen(&state, &address, true, key.as_ref()).await
}

/// Unfreeze an account
#[utoipa::path(
    post, path = "/v1/accounts/{address}/unfreeze", tag = "accounts",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result"), ("address" = String, Path, example = "users:42:wallet")),
    security(("api_key" = [])),
    responses(
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
        (status = 200, body = AccountResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn unfreeze(
    State(state): State<AppState>,
    PathParams(address): PathParams<String>,
    Mutation { key, .. }: Mutation<EmptyRequest>,
) -> Result<Response, ApiError> {
    set_frozen(&state, &address, false, key.as_ref()).await
}

async fn set_frozen(
    state: &AppState,
    address: &str,
    frozen: bool,
    key: Option<&IdempotencyKey>,
) -> Result<Response, ApiError> {
    let address = parse::<AccountAddress>("address", address)?;
    let operation = state.store.set_frozen(&address, frozen, key);
    let replayed = match idempotency::run(&state.store, key, operation).await? {
        Outcome::Done(()) => {
            tracing::info!(account = %address, frozen, "account freeze changed");
            false
        }
        Outcome::Replay { .. } => true,
    };
    Ok(idempotency::respond(
        account(state, &address, None).await?,
        replayed,
    ))
}

async fn account(
    state: &AppState,
    address: &AccountAddress,
    at: Option<TxId>,
) -> Result<Json<AccountResponse>, ApiError> {
    state
        .store
        .account(address, at)
        .await?
        .map(|account| Json(account.into()))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "ACCOUNT_NOT_FOUND",
                "this account has no postings, flags or metadata",
            )
        })
}
