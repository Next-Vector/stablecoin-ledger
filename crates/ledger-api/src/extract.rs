//! Request extractors whose rejections use the error envelope instead of axum's plain text.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// A JSON body; an empty body reads as `{}` so bodies with only optional fields can be omitted.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| ApiError::validation(rejection.body_text()))?;
        let raw: &[u8] = if bytes.is_empty() { b"{}" } else { &bytes };
        serde_json::from_slice(raw)
            .map(JsonBody)
            .map_err(|error| ApiError::validation(format!("invalid JSON body: {error}")))
    }
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
