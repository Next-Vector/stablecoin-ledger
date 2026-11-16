//! The /v1 journal routes (spec §8). Reads and script dry-runs need the `read` role, journal
//! writes and script registration `write`, and freezing accounts `admin`. Handlers stay thin:
//! parse input, call `PgStore`, shape the reply.

pub(crate) mod accounts;
pub(crate) mod dto;
pub(crate) mod holds;
mod idempotency;
pub(crate) mod scripts;
pub(crate) mod transactions;

use axum::Router;
use axum::routing::{get, post};
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
        .route("/v1/scripts/dry-run", post(scripts::dry_run));
    let write = Router::new()
        .route("/v1/transactions", post(transactions::create_transaction))
        .route("/v1/transactions/{id}/revert", post(transactions::revert))
        .route("/v1/holds", post(holds::create_hold))
        .route("/v1/holds/{id}/capture", post(holds::capture))
        .route("/v1/holds/{id}/void", post(holds::void))
        .route("/v1/scripts", post(scripts::register_script));
    let admin = Router::new()
        .route("/v1/accounts/{address}/freeze", post(accounts::freeze))
        .route("/v1/accounts/{address}/unfreeze", post(accounts::unfreeze));
    protect(read, state, Role::Read)
        .merge(protect(write, state, Role::Write))
        .merge(protect(admin, state, Role::Admin))
}
