//! API key authentication (NFR-09). Deny by default: a protected route needs a valid
//! `Authorization: Bearer <key>` whose role allows the route's required role.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ledger_store::{AuthFailure, Role, authenticate};

use crate::AppState;
use crate::error::ApiError;

/// Puts every route of `router` behind API key authentication requiring at least `required`.
/// On success the [`ledger_store::Authenticated`] caller is added to the request extensions.
pub fn protect(router: Router<AppState>, state: &AppState, required: Role) -> Router<AppState> {
    router.route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        move |State(state): State<AppState>, request: Request, next: Next| {
            check(state, required, request, next)
        },
    ))
}

async fn check(state: AppState, required: Role, mut request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(presented) = presented else {
        tracing::warn!(
            reason = "missing bearer credentials",
            "authentication failed"
        );
        return unauthorized();
    };

    match authenticate(&state.pool, presented).await {
        Ok(Ok(caller)) if caller.role.allows(required) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Ok(Ok(caller)) => {
            tracing::warn!(key_id = %caller.key_id, role = caller.role.as_str(), required = required.as_str(), "authorization denied");
            ApiError::new(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "this API key's role does not allow this operation",
            )
            .into_response()
        }
        Ok(Err(failure)) => {
            let (reason, key_id) = match &failure {
                AuthFailure::Malformed => ("malformed key", ""),
                AuthFailure::UnknownKey(id) => ("unknown key", id.as_str()),
                AuthFailure::WrongSecret(id) => ("wrong secret", id.as_str()),
                AuthFailure::Revoked(id) => ("revoked key", id.as_str()),
            };
            tracing::warn!(reason, key_id, "authentication failed");
            unauthorized()
        }
        Err(error) => {
            tracing::error!(%error, "authentication lookup failed");
            ApiError::internal().into_response()
        }
    }
}

fn unauthorized() -> Response {
    let mut response = ApiError::new(
        StatusCode::UNAUTHORIZED,
        "UNAUTHORIZED",
        "a valid API key is required: Authorization: Bearer <key>",
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}
