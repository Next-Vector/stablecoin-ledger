//! Request extractors whose rejections use the error envelope instead of axum's plain text.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use ledger_store::{IdempotencyKey, InvalidIdempotencyKey};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::ApiError;

/// The header carrying the client's idempotency key (FR-10).
pub const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// A mutating request: its JSON body (an empty body reads as `{}`, so bodies with only optional
/// fields can be omitted) and its optional `Idempotency-Key`, bound to a hash of the request.
pub struct Mutation<T> {
    pub body: T,
    pub key: Option<IdempotencyKey>,
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Mutation<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let raw_key = match request.headers().get(IDEMPOTENCY_KEY) {
            Some(value) => Some(
                value
                    .to_str()
                    .map_err(|_| ApiError::invalid_field("Idempotency-Key", InvalidIdempotencyKey))?
                    .to_owned(),
            ),
            None => None,
        };
        let target = format!("{} {}", request.method(), request.uri().path());
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| ApiError::validation(rejection.body_text()))?;
        let raw: &[u8] = if bytes.is_empty() { b"{}" } else { &bytes };
        let invalid =
            |error: serde_json::Error| ApiError::validation(format!("invalid JSON body: {error}"));
        let value: Value = serde_json::from_slice(raw).map_err(invalid)?;
        let key = raw_key
            .map(|key| IdempotencyKey::new(&key, request_hash(&target, &value)))
            .transpose()
            .map_err(|error| ApiError::invalid_field("Idempotency-Key", error))?;
        let body = T::deserialize(value).map_err(invalid)?;
        Ok(Self { body, key })
    }
}

/// SHA-256 of the method, path and canonical JSON body. serde_json orders object keys, so the
/// same request with other key order or whitespace hashes the same.
fn request_hash(target: &str, body: &Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(target.as_bytes());
    hasher.update([0]);
    hasher.update(body.to_string().as_bytes());
    hasher.finalize().into()
}

/// Path parameters.
pub struct PathParams<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for PathParams<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| PathParams(value))
            .map_err(|rejection| ApiError::validation(rejection.body_text()))
    }
}

/// Query string parameters.
pub struct QueryParams<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for QueryParams<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| QueryParams(value))
            .map_err(|rejection| ApiError::validation(rejection.body_text()))
    }
}
