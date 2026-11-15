//! NFR-08: balances can be fully rebuilt from the posting journal.

mod common;

use ledger_core::{AccountAddress, AssetId, Balance, LedgerStore, Metadata, Posting, Transaction};
use ledger_store::PgStore;
use sqlx::PgPool;

fn tx(postings: &[(&str, &str, u128, &str)]) -> Transaction {
    let postings = postings
        .iter()
        .map(|&(from, to, amount, asset)| {
            Posting::new(
                from.parse().unwrap(),
                to.parse().unwrap(),
                amount,
                asset.parse().unwrap(),
            )
            .unwrap()
        })
        .collect();
    Transaction::new(postings, Metadata::new()).unwrap()
}

/// A small journal across two assets, including a fully spent account (zero balance).
async fn seeded() -> (PgStore, PgPool) {
    let pool = common::fresh_database().await;
    let store = PgStore::new(pool.clone());
    store
        .apply(tx(&[
            ("world", "users:1:wallet", 100, "USDC@ethereum/6"),
            ("world", "users:2:wallet", u128::MAX, "DAI@ethereum/18"),
        ]))
        .await
        .unwrap();
    store
        .apply(tx(&[
            (
                "users:1:wallet",
                "merchants:7:wallet",
                60,
                "USDC@ethereum/6",
            ),
            ("users:1:wallet", "platform:fees", 40, "USDC@ethereum/6"),
        ]))
        .await
        .unwrap();
    (store, pool)
}

async fn all_balances(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT account, asset, balance::text FROM balances ORDER BY account, asset")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn consistent_balances_need_no_correction() {
    let (_, pool) = seeded().await;
    let before = all_balances(&pool).await;
    assert_eq!(ledger_store::rebuild_balances(&pool).await.unwrap(), 0);
    assert_eq!(all_balances(&pool).await, before);
}

#[tokio::test]
async fn rebuild_restores_corrupted_missing_and_bogus_rows() {
    let (store, pool) = seeded().await;
    let expected = all_balances(&pool).await;

    sqlx::raw_sql(
        "UPDATE balances SET balance = 999 WHERE account = 'merchants:7:wallet'; \
         DELETE FROM balances WHERE account = 'platform:fees'; \
         UPDATE balances SET balance = -1 WHERE account = 'users:1:wallet'; \
         INSERT INTO balances (account, asset, balance) VALUES ('users:9:wallet', 'USDC@ethereum/6', 5);",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(ledger_store::rebuild_balances(&pool).await.unwrap(), 4);

    // users:9 never moved money: its bogus row is reset to zero rather than deleted.
    let mut rebuilt = all_balances(&pool).await;
    rebuilt.retain(|(account, _, _)| account != "users:9:wallet");
    assert_eq!(rebuilt, expected);
    let users9: AccountAddress = "users:9:wallet".parse().unwrap();
    let usdc: AssetId = "USDC@ethereum/6".parse().unwrap();
    assert_eq!(store.balance(&users9, &usdc).await.unwrap(), Balance::ZERO);
}

#[tokio::test]
async fn rebuilt_balances_keep_the_full_u128_range() {
    let (store, pool) = seeded().await;
    sqlx::raw_sql("UPDATE balances SET balance = 0 WHERE asset = 'DAI@ethereum/18'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(ledger_store::rebuild_balances(&pool).await.unwrap(), 2);

    let dai: AssetId = "DAI@ethereum/18".parse().unwrap();
    let users2: AccountAddress = "users:2:wallet".parse().unwrap();
    assert_eq!(
        store.balance(&users2, &dai).await.unwrap(),
        Balance::ZERO.credit(u128::MAX).unwrap()
    );
    assert_eq!(
        store.balance(&AccountAddress::world(), &dai).await.unwrap(),
        Balance::ZERO.debit(u128::MAX).unwrap()
    );
}
