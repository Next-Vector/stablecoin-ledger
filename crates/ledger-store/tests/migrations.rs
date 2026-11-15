//! Integration tests for the ledger schema (spec §9) against a real PostgreSQL.

mod common;

use sqlx::{AssertSqlSafe, PgPool};

const INSERT_POSTING: &str = "INSERT INTO postings (tx_id, idx, source, destination, asset, amount) \
                              VALUES ($1, $2, 'world', 'users:1:wallet', 'USDC@ethereum/6', CAST($3 AS NUMERIC))";

async fn insert_transaction(pool: &PgPool) -> i64 {
    let (id,): (i64,) = sqlx::query_as("INSERT INTO transactions DEFAULT VALUES RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(INSERT_POSTING)
        .bind(id)
        .bind(0_i16)
        .bind("100")
        .execute(pool)
        .await
        .unwrap();
    id
}

/// Runs a statement the schema must refuse and returns the database error message.
/// The SQL is a fixed test statement, never user input.
async fn error_of(pool: &PgPool, sql: String) -> String {
    sqlx::raw_sql(AssertSqlSafe(sql.clone()))
        .execute(pool)
        .await
        .expect_err(&format!("expected `{sql}` to fail"))
        .to_string()
}

#[tokio::test]
async fn creates_the_core_tables() {
    let pool = common::fresh_database().await;
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name::text FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let tables: Vec<String> = tables.into_iter().map(|(name,)| name).collect();
    for expected in ["accounts", "balances", "holds", "postings", "transactions"] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}, found {tables:?}"
        );
    }
}

#[tokio::test]
async fn amounts_cover_the_full_u128_range() {
    let pool = common::fresh_database().await;
    let id = insert_transaction(&pool).await;
    let max = u128::MAX.to_string();
    sqlx::query(INSERT_POSTING)
        .bind(id)
        .bind(1_i16)
        .bind(&max)
        .execute(&pool)
        .await
        .unwrap();
    let (stored,): (String,) =
        sqlx::query_as("SELECT amount::text FROM postings WHERE tx_id = $1 AND idx = 1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, max);
}

#[tokio::test]
async fn postings_reject_non_positive_amounts() {
    let pool = common::fresh_database().await;
    let id = insert_transaction(&pool).await;
    let error = sqlx::query(INSERT_POSTING)
        .bind(id)
        .bind(1_i16)
        .bind("0")
        .execute(&pool)
        .await
        .expect_err("a zero amount must be rejected")
        .to_string();
    assert!(error.contains("check constraint"), "{error}");
}

#[tokio::test]
async fn transactions_and_postings_are_append_only() {
    let pool = common::fresh_database().await;
    let id = insert_transaction(&pool).await;
    for sql in [
        format!("UPDATE transactions SET metadata = '{{\"x\":\"y\"}}' WHERE id = {id}"),
        format!("DELETE FROM transactions WHERE id = {id}"),
        "TRUNCATE transactions CASCADE".to_owned(),
        format!("UPDATE postings SET amount = 1 WHERE tx_id = {id}"),
        format!("DELETE FROM postings WHERE tx_id = {id}"),
        "TRUNCATE postings".to_owned(),
    ] {
        let error = error_of(&pool, sql.clone()).await;
        assert!(
            error.contains("append-only"),
            "`{sql}` failed with an unexpected error: {error}"
        );
    }
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM postings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_transaction_can_be_reverted_only_once() {
    let pool = common::fresh_database().await;
    let original = insert_transaction(&pool).await;
    let revert = || sqlx::query("INSERT INTO transactions (reverts) VALUES ($1)").bind(original);
    revert().execute(&pool).await.unwrap();
    let error = revert()
        .execute(&pool)
        .await
        .expect_err("second reversal must fail")
        .to_string();
    assert!(error.contains("duplicate key"), "{error}");
}

#[tokio::test]
async fn hold_status_is_a_closed_set() {
    let pool = common::fresh_database().await;
    let insert = |status: &'static str| {
        sqlx::query(
            "INSERT INTO holds (owner, asset, amount, status) \
             VALUES ('users:1:wallet', 'USDC@ethereum/6', 10, $1)",
        )
        .bind(status)
    };
    for status in ["active", "captured", "voided", "expired"] {
        insert(status).execute(&pool).await.unwrap();
    }
    let error = insert("bogus")
        .execute(&pool)
        .await
        .expect_err("unknown status must fail")
        .to_string();
    assert!(error.contains("check constraint"), "{error}");
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let pool = common::fresh_database().await;
    ledger_store::migrate(&pool).await.unwrap();
}
