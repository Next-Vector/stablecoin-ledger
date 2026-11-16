//! Transactions: create (FR-01, FR-02), read and list (FR-06), revert (FR-03).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use ledger_core::{AccountAddress, AssetId, TxId};
use ledger_store::{HistoryFilter, IdempotencyKey, OpError, Page};

use super::dto::{
    CreateTransactionRequest, ListTransactionsQuery, MetadataRequest, TransactionInput,
    TransactionPageResponse, TransactionResponse, parse,
};
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody, transaction_not_found};
use crate::extract::{Mutation, PathParams, QueryParams};

/// Create a transaction
///
/// From `postings`, or from a ledger script (`script` inline, or a stored `script_name`) with
/// `vars`. All postings apply atomically, or none do. A script run records its source and
/// variables in the metadata (`script.source`, `script.vars`, and for stored scripts
/// `script.name` and `script.version`).
#[utoipa::path(
    post, path = "/v1/transactions", tag = "transactions",
    request_body = CreateTransactionRequest,
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    security(("api_key" = [])),
    responses(
        (status = 201, body = TransactionResponse),
        (status = 400, description = "VALIDATION_ERROR or SCRIPT_INVALID", body = ErrorBody),
        (status = 404, description = "SCRIPT_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS, ACCOUNT_FROZEN, BALANCE_OVERFLOW or SCRIPT_REJECTED", body = ErrorBody),
    )
)]
pub async fn create_transaction(
    State(state): State<AppState>,
    Mutation { body, key }: Mutation<CreateTransactionRequest>,
) -> Result<Response, ApiError> {
    match body.into_input()? {
        TransactionInput::Postings(tx) => {
            let operation = state.store.create_transaction(tx, key.as_ref());
            created(&state, key.as_ref(), operation).await
        }
        TransactionInput::Script(run) => {
            let (script, vars, audit) = super::scripts::load(&state, run).await?;
            let operation = state.store.run_script(&script, &vars, audit, key.as_ref());
            created(&state, key.as_ref(), operation).await
        }
    }
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
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result"), ("id" = u64, Path, description = "Transaction id")),
    request_body(content = Option<MetadataRequest>, description = "Optional metadata of the reversal"),
    security(("api_key" = [])),
    responses(
        (status = 201, description = "The reversal", body = TransactionResponse),
        (status = 404, description = "TRANSACTION_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT or ALREADY_REVERTED", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS or ACCOUNT_FROZEN", body = ErrorBody),
    )
)]
pub async fn revert(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    Mutation { body, key }: Mutation<MetadataRequest>,
) -> Result<Response, ApiError> {
    let operation = state.store.revert(TxId(id), body.metadata, key.as_ref());
    created(&state, key.as_ref(), operation).await
}

/// Runs an operation that writes one transaction under its idempotency key and answers
/// `201 Created` with that transaction (on a replay too).
async fn created<E: Into<ApiError>>(
    state: &AppState,
    key: Option<&IdempotencyKey>,
    operation: impl Future<Output = Result<TxId, OpError<E>>>,
) -> Result<Response, ApiError> {
    let (id, replayed) = match idempotency::run(&state.store, key, operation).await? {
        Outcome::Done(id) => (id, false),
        Outcome::Replay(resources) => (idempotency::recorded(resources.transaction)?, true),
    };
    let response = (StatusCode::CREATED, Json(recorded(state, id).await?));
    Ok(idempotency::respond(response, replayed))
}

async fn recorded(state: &AppState, id: TxId) -> Result<TransactionResponse, ApiError> {
    state
        .store
        .transaction(id)
        .await?
        .map(Into::into)
        .ok_or_else(transaction_not_found)
}
