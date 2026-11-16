//! Deposit addresses (FR-20): register an address issued and watched outside the ledger (by the
//! chain scanning module) for the wallet account its deposits credit. The ledger does not know any
//! chain's address format, so the address is kept as an opaque string.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use ledger_core::AccountAddress;
use ledger_store::DepositAddress;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::dto::parse;
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody};
use crate::extract::Mutation;

/// The longest address accepted (bytes); the schema allows the same.
const MAX_ADDRESS_LEN: usize = 255;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterDepositAddressRequest {
    /// Network of a registered asset, e.g. `ethereum`.
    #[schema(example = "ethereum")]
    pub network: String,
    /// The address in its network's own format, stored exactly as given (1–255 characters, no
    /// whitespace).
    #[schema(example = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed")]
    pub address: String,
    /// The wallet account deposits credit (`…:wallet`); they arrive in `…:pending` first.
    #[schema(example = "users:42:wallet")]
    pub account: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DepositAddressResponse {
    pub id: u64,
    pub network: String,
    /// Exactly as registered.
    pub address: String,
    pub account: String,
    /// Unix seconds.
    pub created_at: i64,
}

impl From<DepositAddress> for DepositAddressResponse {
    fn from(registered: DepositAddress) -> Self {
        Self {
            id: registered.id,
            network: registered.network,
            address: registered.address,
            account: registered.account.to_string(),
            created_at: registered.created_at,
        }
    }
}

/// Register a deposit address
///
/// Maps an address on a network to the wallet account its deposits credit. An address belongs to
/// one account, and an account has one address per network.
#[utoipa::path(
    post, path = "/v1/deposit-addresses", tag = "deposits",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    request_body = RegisterDepositAddressRequest,
    security(("api_key" = [])),
    responses(
        (status = 201, body = DepositAddressResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 409, description = "DEPOSIT_ADDRESS_TAKEN, ACCOUNT_HAS_DEPOSIT_ADDRESS or IDEMPOTENCY_CONFLICT", body = ErrorBody),
        (status = 422, description = "UNKNOWN_NETWORK: no asset is registered on the network", body = ErrorBody),
    )
)]
pub async fn register_deposit_address(
    State(state): State<AppState>,
    Mutation { body, key }: Mutation<RegisterDepositAddressRequest>,
) -> Result<Response, ApiError> {
    let address = body.address.as_str();
    if address.is_empty()
        || address.len() > MAX_ADDRESS_LEN
        || address.contains(char::is_whitespace)
    {
        return Err(ApiError::invalid_field(
            "address",
            format!("must be 1 to {MAX_ADDRESS_LEN} bytes without whitespace"),
        ));
    }
    let account = parse::<AccountAddress>("account", &body.account)?;
    if !account.as_str().ends_with(":wallet") {
        return Err(ApiError::invalid_field(
            "account",
            "must be a wallet account (…:wallet)",
        ));
    }
    let operation =
        state
            .store
            .register_deposit_address(&body.network, address, &account, key.as_ref());
    let (registered, replayed) =
        match idempotency::run(&state.store, key.as_ref(), operation).await? {
            Outcome::Done(registered) => (registered, false),
            Outcome::Replay(resources) => {
                let id = idempotency::recorded(resources.deposit_address)?;
                (
                    idempotency::recorded(state.store.deposit_address(id).await?)?,
                    true,
                )
            }
        };
    Ok(idempotency::respond(
        (
            StatusCode::CREATED,
            Json(DepositAddressResponse::from(registered)),
        ),
        replayed,
    ))
}
