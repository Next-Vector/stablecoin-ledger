//! PostgreSQL storage for the ledger journal and materialized balances.

use sqlx::PgPool;
use sqlx::migrate::MigrateError;

/// Applies all pending schema migrations embedded from `migrations/`.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
