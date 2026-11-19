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
use utoipa::{IntoParams, ToSchema};

use super::dto::parse;
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody};
use crate::extract::{Mutation, QueryParams};

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

#[derive(Debug, Serialize, ToSchema)]
pub struct DepositAddressPageResponse {
    pub items: Vec<DepositAddressResponse>,
    /// Pass as `cursor` for the next page; absent on the last one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListDepositAddressesQuery {
    /// Only this network.
    pub network: Option<String>,
    /// Only this address, matched exactly (letter case included).
    pub address: Option<String>,
    /// Only this wallet account.
    pub account: Option<String>,
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<u64>,
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

/// List deposit addresses
///
/// Registrations in registration order, filtered by network, address (exact match) and account.
/// Looking up one address on one network returns at most one item: the account it deposits into.
#[utoipa::path(
    get, path = "/v1/deposit-addresses", tag = "deposits",
    params(ListDepositAddressesQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = DepositAddressPageResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn list_deposit_addresses(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<ListDepositAddressesQuery>,
) -> Result<Json<DepositAddressPageResponse>, ApiError> {
    let limit = query.limit.unwrap_or(ledger_store::Page::DEFAULT_LIMIT);
    ledger_store::Page::new(limit, None)
        .map_err(|error| ApiError::invalid_field("limit", error))?;
    let account = query
        .account
        .as_deref()
        .map(|raw| parse::<AccountAddress>("account", raw))
        .transpose()?;
    let (items, next_cursor) = state
        .store
        .deposit_addresses(
            query.network.as_deref(),
            query.address.as_deref(),
            account.as_ref(),
            query.cursor,
            limit,
        )
        .await?;
    Ok(Json(DepositAddressPageResponse {
        items: items
            .into_iter()
            .map(DepositAddressResponse::from)
            .collect(),
        next_cursor,
    }))
}
