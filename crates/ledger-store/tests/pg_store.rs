//! Integration tests for PgStore: atomic apply with materialized balances (NFR-01, NFR-02, NFR-06).

mod common;

use ledger_core::{
    AccountAddress, ApplyError, AssetId, Balance, LedgerStore, Metadata, Posting, StoreError,
    Transaction,
};
use ledger_store::PgStore;
use sqlx::PgPool;

fn account(raw: &str) -> AccountAddress {
    raw.parse().unwrap()
}

fn usdc() -> AssetId {
    "USDC@ethereum/6".parse().unwrap()
}

fn tx(postings: &[(&str, &str, u128)]) -> Transaction {
    tx_with(postings, Metadata::new())
}

fn tx_with(postings: &[(&str, &str, u128)], metadata: Metadata) -> Transaction {
    let postings = postings
        .iter()
        .map(|&(from, to, amount)| {
            Posting::new(account(from), account(to), amount, usdc()).unwrap()
        })
        .collect();
    Transaction::new(postings, metadata).unwrap()
}

fn credit(value: u128) -> Balance {
    Balance::ZERO.credit(value).unwrap()
}

async fn balance(store: &PgStore, raw: &str) -> Balance {
    store.balance(&account(raw), &usdc()).await.unwrap()
}

async fn count(pool: &PgPool, table: &'static str) -> i64 {
    let sql = match table {
        "transactions" => "SELECT count(*) FROM transactions",
        "postings" => "SELECT count(*) FROM postings",
        _ => unreachable!(),
    };
    sqlx::query_as::<_, (i64,)>(sql)
        .fetch_one(pool)
        .await
        .unwrap()
        .0
}

async fn store() -> (PgStore, PgPool) {
    let pool = common::fresh_database().await;
    (PgStore::new(pool.clone()), pool)
}

#[tokio::test]
async fn applies_a_transaction_and_updates_materialized_balances() {
    let (store, pool) = store().await;
    let first = store
        .apply(tx(&[("world", "users:1:wallet", 100)]))
        .await
        .unwrap();
    let second = store
        .apply(tx(&[
            ("users:1:wallet", "merchants:7:wallet", 99),
            ("users:1:wallet", "platform:fees", 1),
        ]))
        .await
        .unwrap();

    assert!(first < second);
    assert_eq!(balance(&store, "users:1:wallet").await, Balance::ZERO);
    assert_eq!(balance(&store, "merchants:7:wallet").await, credit(99));
    assert_eq!(balance(&store, "platform:fees").await, credit(1));
    assert_eq!(balance(&store, "users:9:wallet").await, Balance::ZERO);
    assert_eq!(count(&pool, "transactions").await, 2);
    assert_eq!(count(&pool, "postings").await, 3);
}

#[tokio::test]
async fn world_may_go_negative() {
    let (store, _) = store().await;
    store
        .apply(tx(&[("world", "users:1:wallet", 100)]))
        .await
        .unwrap();
    assert_eq!(
        balance(&store, "world").await,
        Balance::ZERO.debit(100).unwrap()
    );
}

#[tokio::test]
async fn insufficient_funds_rolls_back_everything() {
    let (store, pool) = store().await;
    store
        .apply(tx(&[("world", "users:1:wallet", 100)]))
        .await
        .unwrap();
    let result = store
        .apply(tx(&[
            ("users:1:wallet", "users:2:wallet", 60),
            ("users:1:wallet", "users:3:wallet", 60),
        ]))
        .await;

    assert!(matches!(
        result,
        Err(StoreError::Apply(ApplyError::InsufficientFunds { ref account, .. })) if account.as_str() == "users:1:wallet"
    ));
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
    assert_eq!(balance(&store, "users:2:wallet").await, Balance::ZERO);
    assert_eq!(count(&pool, "transactions").await, 1);
    assert_eq!(count(&pool, "postings").await, 1);
}

#[tokio::test]
async fn frozen_account_cannot_send_but_can_receive() {
    let (store, pool) = store().await;
    store
        .apply(tx(&[("world", "users:1:wallet", 100)]))
        .await
        .unwrap();
    sqlx::query("INSERT INTO accounts (address, frozen) VALUES ('users:1:wallet', true)")
        .execute(&pool)
        .await
        .unwrap();

    let result = store
        .apply(tx(&[("users:1:wallet", "users:2:wallet", 1)]))
        .await;
    assert!(matches!(
        result,
        Err(StoreError::Apply(ApplyError::AccountFrozen(_)))
    ));
    store
        .apply(tx(&[("world", "users:1:wallet", 5)]))
        .await
        .unwrap();
    assert_eq!(balance(&store, "users:1:wallet").await, credit(105));
}

#[tokio::test]
async fn stores_the_full_u128_range_and_rejects_overflow() {
    let (store, _) = store().await;
    store
        .apply(tx(&[("world", "users:1:wallet", u128::MAX)]))
        .await
        .unwrap();
    assert_eq!(balance(&store, "users:1:wallet").await, credit(u128::MAX));
    assert_eq!(
        balance(&store, "world").await,
        Balance::ZERO.debit(u128::MAX).unwrap()
    );

    let result = store.apply(tx(&[("world", "users:2:wallet", 1)])).await;
    assert!(matches!(
        result,
        Err(StoreError::Apply(ApplyError::Overflow))
    ));
}

#[tokio::test]
async fn stores_metadata_and_bumps_balance_versions() {
    let (store, pool) = store().await;
    let metadata = Metadata::from([("order_id".to_owned(), "8812".to_owned())]);
    let id = store
        .apply(tx_with(&[("world", "users:1:wallet", 10)], metadata))
        .await
        .unwrap();
    store
        .apply(tx(&[("world", "users:1:wallet", 5)]))
        .await
        .unwrap();

    let (order_id,): (String,) =
        sqlx::query_as("SELECT metadata->>'order_id' FROM transactions WHERE id = $1")
            .bind(id.0 as i64)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(order_id, "8812");
    let (version,): (i64,) =
        sqlx::query_as("SELECT version FROM balances WHERE account = 'users:1:wallet'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, 2);
}

#[tokio::test]
async fn crossing_transfers_do_not_deadlock() {
    let (store, _) = store().await;
    store
        .apply(tx(&[
            ("world", "users:1:wallet", 1_000),
            ("world", "users:2:wallet", 1_000),
        ]))
        .await
        .unwrap();

    let tasks: Vec<_> = (0..100)
        .map(|i| {
            let store = store.clone();
            let (from, to) = if i % 2 == 0 {
                ("users:1:wallet", "users:2:wallet")
            } else {
                ("users:2:wallet", "users:1:wallet")
            };
            tokio::spawn(async move {
                store
                    .apply(tx(&[(from, to, 1), ("world", "platform:fees", 1)]))
                    .await
            })
        })
        .collect();
    for task in tasks {
        task.await
            .unwrap()
            .expect("no transfer may fail or deadlock");
    }

    assert_eq!(balance(&store, "users:1:wallet").await, credit(1_000));
    assert_eq!(balance(&store, "users:2:wallet").await, credit(1_000));
    assert_eq!(balance(&store, "platform:fees").await, credit(100));
}
