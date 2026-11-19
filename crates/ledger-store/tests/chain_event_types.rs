//! Migration 20261117100000 (#66): chain event types are stripped from existing subscriptions,
//! and a subscription left with none is deleted, since the ledger can never emit what it had.

mod common;

use ledger_store::PgStore;

const MIGRATION: &str = include_str!("../migrations/20261117100000_remove_chain_event_types.sql");

async fn endpoint(pool: &sqlx::PgPool, url: &str, types: &[&str]) -> i64 {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO webhook_endpoints (url, event_types, secret) VALUES ($1, $2, '\\x00') RETURNING id",
    )
    .bind(url)
    .bind(types)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn chain_event_types_are_stripped_from_subscriptions() {
    let pool = common::fresh_database().await;
    let mixed = endpoint(
        &pool,
        "https://mixed.example/hooks",
        &[
            "transaction.created",
            "deposit.seen",
            "hold.expired",
            "reconciliation.mismatch",
        ],
    )
    .await;
    let chain_only = endpoint(
        &pool,
        "https://chain.example/hooks",
        &["withdrawal.failed", "deposit.confirmed"],
    )
    .await;
    let ledger_only = endpoint(&pool, "https://ledger.example/hooks", &["account.frozen"]).await;

    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();

    let rows: Vec<(i64, Vec<String>)> =
        sqlx::query_as("SELECT id, event_types FROM webhook_endpoints ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                mixed,
                vec!["transaction.created".to_owned(), "hold.expired".to_owned()]
            ),
            (ledger_only, vec!["account.frozen".to_owned()]),
        ]
    );
    assert!(!rows.iter().any(|(id, _)| *id == chain_only));
    // Every remaining subscription loads again.
    let (endpoints, _) = PgStore::new(pool).webhooks(None, 50).await.unwrap();
    assert_eq!(endpoints.len(), 2);
}
