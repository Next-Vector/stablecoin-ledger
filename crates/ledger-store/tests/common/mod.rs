//! Shared setup for integration tests: a fresh, migrated schema in the run-integration database.

use sqlx::PgPool;

/// Connects to `DATABASE_URL`, drops the whole `public` schema and re-runs all migrations.
/// Tests share one database, so they must run on a single thread (see scripts/test-integration.sh).
pub async fn fresh_database() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to the integration database");
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .expect("reset schema");
    ledger_store::migrate(&pool).await.expect("run migrations");
    pool
}
