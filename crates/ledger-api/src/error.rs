//! The single error envelope every route returns (spec §8):
//! `{"error": {"code": "INSUFFICIENT_FUNDS", "message": "…", "details": {…}}}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ledger_core::{ApplyError, HoldError, RevertError, StoreError};
use ledger_script::{CompileError, ParseError};
use ledger_store::{OpError, RegisterError, ScriptError};
use serde::Serialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Stable machine-readable code, e.g. `NOT_FOUND`.
    pub code: String,
    /// Human-readable explanation; never contains internal details.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub details: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorBody {
                error: ErrorDetail {
                    code: code.to_owned(),
                    message: message.into(),
                    details: None,
                },
            },
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.body.error.details = Some(details);
        self
    }

    /// The request is malformed or breaks an input rule (`400 VALIDATION_ERROR`).
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "VALIDATION_ERROR", message)
    }

    /// Input `field` failed validation; `reason` explains why.
    pub fn invalid_field(field: &str, reason: impl std::fmt::Display) -> Self {
        Self::validation(format!("{field}: {reason}"))
            .with_details(json!({ "field": field, "reason": reason.to_string() }))
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "no route matches this path",
        )
    }

    /// An unexpected failure; the details go to the logs, never to the client.
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "an internal error occurred",
        )
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "METHOD_NOT_ALLOWED",
            "this method is not allowed on this path",
        )
    }
}

impl From<ApplyError> for ApiError {
    fn from(error: ApplyError) -> Self {
        let message = error.to_string();
        match error {
            ApplyError::InsufficientFunds { account, asset } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "INSUFFICIENT_FUNDS",
                message,
            )
            .with_details(json!({ "account": account.as_str(), "asset": asset.to_string() })),
            ApplyError::AccountFrozen(account) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "ACCOUNT_FROZEN", message)
                    .with_details(json!({ "account": account.as_str() }))
            }
            ApplyError::Overflow => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "BALANCE_OVERFLOW",
                message,
            ),
        }
    }
}

impl From<RevertError> for ApiError {
    fn from(error: RevertError) -> Self {
        match error {
            RevertError::NotFound(_) => transaction_not_found(),
            RevertError::AlreadyReverted(_) => Self::new(
                StatusCode::CONFLICT,
                "ALREADY_REVERTED",
                "this transaction has already been reverted",
            ),
            RevertError::Apply(rule) => rule.into(),
        }
    }
}

impl From<HoldError> for ApiError {
    fn from(error: HoldError) -> Self {
        let message = error.to_string();
        match error {
            HoldError::NotFound(_) => Self::new(
                StatusCode::NOT_FOUND,
                "HOLD_NOT_FOUND",
                "no hold has this id",
            ),
            HoldError::NotActive(_, status) => Self::new(
                StatusCode::CONFLICT,
                "HOLD_NOT_ACTIVE",
                format!("the hold is {}, not active", status.as_str()),
            )
            .with_details(json!({ "status": status.as_str() })),
            HoldError::ExceedsHold { requested, held } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "HOLD_AMOUNT_EXCEEDED",
                message,
            )
            .with_details(json!({ "requested": requested.to_string(), "held": held.to_string() })),
            HoldError::ZeroAmount => Self::invalid_field("amount", "must be greater than zero"),
            HoldError::Posting(posting) => Self::validation(posting.to_string()),
            HoldError::Apply(rule) => rule.into(),
        }
    }
}

impl From<ScriptError> for ApiError {
    fn from(error: ScriptError) -> Self {
        match error {
            ScriptError::Apply(rule) => rule.into(),
            ScriptError::Compile(error) => error.into(),
        }
    }
}

impl From<CompileError> for ApiError {
    fn from(error: CompileError) -> Self {
        let message = error.to_string();
        match error {
            CompileError::MissingValue(name)
            | CompileError::UndeclaredValue(name)
            | CompileError::InvalidValue { name, .. } => {
                Self::invalid_field(&format!("vars.{name}"), message)
            }
            CompileError::InvalidAccount { .. } => Self::validation(message),
            CompileError::InsufficientFunds {
                asset,
                covered,
                needed,
            } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "INSUFFICIENT_FUNDS",
                message,
            )
            .with_details(json!({
                "asset": asset.to_string(),
                "covered": covered.to_string(),
                "needed": needed.to_string(),
            })),
            CompileError::AssetMismatch { .. }
            | CompileError::Split(_)
            | CompileError::Overflow
            | CompileError::NoPostings
            | CompileError::TooManyPostings(_)
            | CompileError::Posting(_) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "SCRIPT_REJECTED", message)
            }
        }
    }
}

impl From<RegisterError> for ApiError {
    fn from(error: RegisterError) -> Self {
        match error {
            RegisterError::InvalidName => Self::invalid_field("name", error),
            RegisterError::Invalid(parse) => script_invalid(&parse),
        }
    }
}

/// The script does not parse; the details give the position.
pub fn script_invalid(error: &ParseError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "SCRIPT_INVALID", error.to_string()).with_details(
        json!({ "line": error.span.line, "column": error.span.column, "reason": error.message }),
    )
}

pub fn script_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "SCRIPT_NOT_FOUND",
        "no stored script has this name and version",
    )
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Apply(rule) => rule.into(),
            StoreError::Backend(source) => internal(source.as_ref()),
        }
    }
}

impl<E: Into<ApiError>> From<OpError<E>> for ApiError {
    fn from(error: OpError<E>) -> Self {
        match error {
            OpError::Rejected(rule) => rule.into(),
            OpError::KeyReused => idempotency_conflict(),
            OpError::Backend(source) => internal(source.as_ref()),
        }
    }
}

impl From<std::convert::Infallible> for ApiError {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

pub fn idempotency_conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "IDEMPOTENCY_CONFLICT",
        "this Idempotency-Key was already used for a different request",
    )
}

pub fn transaction_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "TRANSACTION_NOT_FOUND",
        "no transaction has this id",
    )
}

/// Logs a backend failure and hides it from the client.
fn internal(source: &(dyn std::error::Error + Send + Sync)) -> ApiError {
    tracing::error!(error = %source, "storage backend failed");
    ApiError::internal()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}
