//! The /v1 journal routes (spec §8). Reads and script dry-runs need the `read` role, journal
//! writes and script registration `write`, and freezing accounts and managing webhooks `admin`. Handlers stay thin:
//! parse input, call `PgStore`, shape the reply.

pub(crate) mod accounts;
pub(crate) mod deposit_addresses;
pub(crate) mod dto;
pub(crate) mod holds;
mod idempotency;
pub(crate) mod scripts;
pub(crate) mod transactions;
pub(crate) mod webhooks;

use axum::Router;
use axum::routing::{get, patch, post};
use ledger_store::Role;

use crate::{AppState, protect};

/// The OpenAPI security scheme name every /v1 operation requires.
pub const SECURITY_SCHEME: &str = "api_key";

pub fn routes(state: &AppState) -> Router<AppState> {
    let read = Router::new()
        .route("/v1/transactions", get(transactions::list_transactions))
        .route("/v1/transactions/{id}", get(transactions::get_transaction))
        .route("/v1/accounts/{address}", get(accounts::get_account))
        .route("/v1/balances", get(accounts::aggregate_balances))
        .route("/v1/scripts", get(scripts::list_scripts))
        .route("/v1/scripts/{name}/versions", get(scripts::script_versions))
        .route("/v1/scripts/dry-run", post(scripts::dry_run))
        .route(
            "/v1/deposit-addresses",
            get(deposit_addresses::list_deposit_addresses),
        );
    let write = Router::new()
        .route("/v1/transactions", post(transactions::create_transaction))
        .route("/v1/transactions/{id}/revert", post(transactions::revert))
        .route("/v1/holds", post(holds::create_hold))
        .route("/v1/holds/{id}/capture", post(holds::capture))
        .route("/v1/holds/{id}/void", post(holds::void))
        .route("/v1/scripts", post(scripts::register_script))
        .route(
            "/v1/deposit-addresses",
            post(deposit_addresses::register_deposit_address),
        );
    let admin = Router::new()
        .route("/v1/accounts/{address}/freeze", post(accounts::freeze))
        .route("/v1/accounts/{address}/unfreeze", post(accounts::unfreeze))
        .route(
            "/v1/webhooks",
            get(webhooks::list_webhooks).post(webhooks::create_webhook),
        )
        .route(
            "/v1/webhooks/{id}",
            patch(webhooks::update_webhook).delete(webhooks::delete_webhook),
        )
        .route(
            "/v1/webhooks/{id}/rotate-secret",
            post(webhooks::rotate_secret),
        )
        .route("/v1/webhooks/{id}/deliveries", get(webhooks::delivery_log))
        .route(
            "/v1/webhook-events/{id}/redeliver",
            post(webhooks::redeliver),
        );
    protect(read, state, Role::Read)
        .merge(protect(write, state, Role::Write))
        .merge(protect(admin, state, Role::Admin))
}
