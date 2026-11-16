//! Transactions: create (FR-01, FR-02), read and list (FR-06), revert (FR-03).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use ledger_core::{AccountAddress, AssetId, LedgerStore, TxId};
use ledger_store::{HistoryFilter, Page};

use super::dto::{
    CreateTransactionRequest, ListTransactionsQuery, MetadataRequest, TransactionPageResponse,
    TransactionResponse, parse,
};
use crate::AppState;
use crate::error::{ApiError, ErrorBody, transaction_not_found};
use crate::extract::{JsonBody, PathParams, QueryParams};

/// Create a transaction
///
/// All postings apply atomically, or none do.
#[utoipa::path(
    post, path = "/v1/transactions", tag = "transactions",
    request_body = CreateTransactionRequest,
    security(("api_key" = [])),
    responses(
        (status = 201, body = TransactionResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS, ACCOUNT_FROZEN or BALANCE_OVERFLOW", body = ErrorBody),
    )
)]
pub async fn create_transaction(
    State(state): State<AppState>,
    JsonBody(request): JsonBody<CreateTransactionRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), ApiError> {
    let id = state.store.apply(request.into_transaction()?).await?;
    Ok((StatusCode::CREATED, Json(recorded(&state, id).await?)))
}

/// Get a transaction
#[utoipa::path(
    get, path = "/v1/transactions/{id}", tag = "transactions",
    params(("id" = u64, Path, description = "Transaction id")),
    security(("api_key" = [])),
    responses(
        (status = 200, body = TransactionResponse),
        (status = 404, description = "TRANSACTION_NOT_FOUND", body = ErrorBody),
    )
)]
pub async fn get_transaction(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
) -> Result<Json<TransactionResponse>, ApiError> {
    Ok(Json(recorded(&state, TxId(id)).await?))
}

/// List transactions
///
/// Newest first, filtered and paginated with a cursor.
#[utoipa::path(
    get, path = "/v1/transactions", tag = "transactions",
    params(ListTransactionsQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = TransactionPageResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn list_transactions(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<ListTransactionsQuery>,
) -> Result<Json<TransactionPageResponse>, ApiError> {
    let metadata = match (query.metadata_key, query.metadata_value) {
        (Some(key), Some(value)) => Some((key, value)),
        (None, None) => None,
        _ => {
            return Err(ApiError::validation(
                "metadata_key and metadata_value must be given together",
            ));
        }
    };
    let filter = HistoryFilter {
        account: query
            .account
            .map(|raw| parse::<AccountAddress>("account", &raw))
            .transpose()?,
        asset: query
            .asset
            .map(|raw| parse::<AssetId>("asset", &raw))
            .transpose()?,
        from: query.from,
        to: query.to,
        metadata,
    };
    let page = Page::new(
        query.limit.unwrap_or(Page::DEFAULT_LIMIT),
        query.cursor.map(TxId),
    )
    .map_err(|error| ApiError::invalid_field("limit", error))?;
    Ok(Json(state.store.transactions(&filter, &page).await?.into()))
}

/// Revert a transaction
///
/// Creates a new transaction with mirrored postings. A transaction can be reverted once.
#[utoipa::path(
    post, path = "/v1/transactions/{id}/revert", tag = "transactions",
    params(("id" = u64, Path, description = "Transaction id")),
    request_body(content = Option<MetadataRequest>, description = "Optional metadata of the reversal"),
    security(("api_key" = [])),
    responses(
        (status = 201, description = "The reversal", body = TransactionResponse),
        (status = 404, description = "TRANSACTION_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "ALREADY_REVERTED", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS or ACCOUNT_FROZEN", body = ErrorBody),
    )
)]
pub async fn revert(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    JsonBody(request): JsonBody<MetadataRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), ApiError> {
    let reversal = state.store.revert(TxId(id), request.metadata).await?;
    Ok((StatusCode::CREATED, Json(recorded(&state, reversal).await?)))
}

async fn recorded(state: &AppState, id: TxId) -> Result<TransactionResponse, ApiError> {
    state
        .store
        .transaction(id)
        .await?
        .map(Into::into)
        .ok_or_else(transaction_not_found)
}
