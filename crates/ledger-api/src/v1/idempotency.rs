//! Running a mutating request under its optional Idempotency-Key (FR-10, FR-11): a repeated
//! request is answered from the resources the first one produced, never executed twice.

use std::future::Future;

use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use ledger_core::{HoldId, TxId};
use ledger_store::{IdempotencyKey, KeyStatus, OpError, PgStore};

use crate::error::{ApiError, idempotency_conflict};

/// Response header set on a replayed answer.
pub const REPLAYED: &str = "idempotent-replayed";

pub enum Outcome<T> {
    /// The operation ran now.
    Done(T),
    /// The key was used before for this same request; these are the resources it produced.
    Replay {
        transaction: Option<TxId>,
        hold: Option<HoldId>,
    },
}

/// Runs `operation` (which must record `key` in its DB transaction) unless the key was used
/// before. A future does nothing until awaited, so a replay never executes `operation`.
pub async fn run<T, E: Into<ApiError>>(
    store: &PgStore,
    key: Option<&IdempotencyKey>,
    operation: impl Future<Output = Result<T, OpError<E>>>,
) -> Result<Outcome<T>, ApiError> {
    let Some(key) = key else {
        return Ok(Outcome::Done(operation.await?));
    };
    if let Some(replay) = previous(store, key).await? {
        return Ok(replay);
    }
    match operation.await {
        Ok(value) => Ok(Outcome::Done(value)),
        // A concurrent request with the same key committed first; ours rolled back.
        Err(OpError::KeyReused) => previous(store, key).await?.ok_or_else(|| {
            tracing::error!(key = key.as_str(), "idempotency key reused but not found");
            ApiError::internal()
        }),
        Err(error) => Err(error.into()),
    }
}

async fn previous<T>(
    store: &PgStore,
    key: &IdempotencyKey,
) -> Result<Option<Outcome<T>>, ApiError> {
    match store.key_status(key).await? {
        KeyStatus::New => Ok(None),
        KeyStatus::Replay { transaction, hold } => Ok(Some(Outcome::Replay { transaction, hold })),
        KeyStatus::Conflict => Err(idempotency_conflict()),
    }
}

/// A resource id a replay must carry for this route; its absence means corrupted records.
pub fn recorded<T>(id: Option<T>) -> Result<T, ApiError> {
    id.ok_or_else(|| {
        tracing::error!("idempotency record lacks the resource of its route");
        ApiError::internal()
    })
}

/// Marks `response` as replayed when it answers a repeated request.
pub fn respond(response: impl IntoResponse, replayed: bool) -> Response {
    let mut response = response.into_response();
    if replayed {
        response
            .headers_mut()
            .insert(REPLAYED, HeaderValue::from_static("true"));
    }
    response
}
